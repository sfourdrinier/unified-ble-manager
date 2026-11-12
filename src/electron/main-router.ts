// src/electron/main-router.ts

import {
  BackendContractError,
  contractError,
  serializeNormalizedError,
  type CleanupFailure,
  type CleanupRecord
} from '../backend-contract/errors'
import {
  ElectronMainArbiterContext,
  type IpcEnvelope,
  type RendererIdentity,
  type RendererLeaseIdentity,
  type TrustedIpcSender
} from '../backend-contract/electron'
import type {
  CharacteristicPath,
  CharacteristicProperties,
  DescriptorPath,
  GattAccessRequirements,
  GattDatabaseChangedEvent,
  GattDescriptorProperties,
  GattServiceRestriction
} from '../backend-contract/gatt'
import type { AttachmentRecord, HostNeutralBackendIdentity } from '../backend-contract/identity'
import {
  byteLimit,
  capacity,
  deadline,
  negotiateVersion,
  negotiateCoreVersions,
  opaqueId,
  ownBytes,
  version,
  versionRange,
  type IpcCompatibilityOffer,
  type IpcVersionAxes,
  type OwnedBytes,
  type SerializableRecord,
  type SerializableValue
} from '../backend-contract/primitives'
import type { SubscriptionOptions } from '../backend-contract/operations'
import { snapshotCapabilityDescriptors } from '../backend-contract/capabilities'
import { serializableRecordsEqual, snapshotSerializableRecord } from '../backend-contract/serializable'
import { snapshotScanPlan } from '../backend-contract/scan-planning'
import type { ScanPlan } from '../backend-contract/scan-planning'
import { decodeIpcScanQuery, encodeIpcScanPlan } from '../ipc/scan-planning'
import { decodePeerQuery, encodePeerRecord } from '../ipc/peer-directory'
import { snapshotPeerReference } from '../public/peer-reference'
import { IPC_ATTACHMENT_STREAM_ID, IPC_CLIENT_COMPATIBILITY_OFFER, ipcAttachmentRecordV2 } from '../ipc/protocol'
import { BleManager, Connection, DiscoveredGattDatabase } from '../manager/ble-manager'
import type {
  ElectronBleIpcEvent,
  ElectronBleIpcRequest,
  ElectronBleIpcSuccessResponse,
  ElectronRendererBootstrap
} from './protocol'
import {
  ElectronRendererStreamRegistry,
  type ElectronEventDelivery,
  type ManagedScan,
  type ManagedSubscription
} from './renderer-stream-registry'
import {
  ElectronConnectionEventStreamRegistry,
  type ManagedConnectionEventSubscription
} from './connection-event-stream-registry'
import { isElectronConnectionEventsStreamHandle, type ElectronConnectionEventsSubscribeResponseV2 } from './protocol'
import { electronRequestByteLength } from './ipc-message-sizing'
import { pairingResponse, isSecurityEvent } from '../ipc/security'
import type { SecurityPairingResponse, SecurityPairingChallenge } from '../backend-contract/security'
import type { BoundedAsyncStream, BoundedAsyncStreamIterator } from '../backend-contract/streams'
import { decodeIpcScanPlatform } from '../ipc/scan-platform'
import { waitForChildCleanup } from '../ipc/cleanup-drain'
import { OrderedDelivery } from '../ipc/ordered-delivery'
import { ElectronOrderedRecordingRegistry } from './ordered-recording-registry'
import { ElectronOrderedRecordingRecovery } from './ordered-recording-recovery'
import { ElectronOrderedMarkerRegistry } from './ordered-marker-registry'
import {
  DesktopRustCoreBackend,
  type DesktopDurablePeerNotificationFailureRecovery
} from '../backends/desktop/desktop-rust-core-provider'
import type { DesktopScanFailureRecovery } from '../backends/desktop/scan-failure-recovery-recording'
import { normalizeContinuationRecording } from '../backend-contract/continuation-setup'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { parseOrderedRecordingHostCleanup, parseOrderedRecordingStopReceipt } from '../core/continuation-recording'
import { isCanonicalIngressOrdinal } from '../core/ordered-notification-contract'
import { accountOrderedDiscard } from '../core/ordered-notification-stream'
import { orderedWireRecord } from '../ipc/ordered-wire'
import type { OrderedNotificationItem, OrderedNotificationSetup } from '../core/ordered-notification-contract'
import type {
  TerminalEvidenceReservation,
  TerminalEvidenceReservationRequest,
  TerminalEvidenceSource
} from '../backend-contract/terminal-evidence'

export type { ElectronEventDelivery } from './renderer-stream-registry'

type MainManager = BleManager<string, HostNeutralBackendIdentity<string>>
type MainConnection = Connection<string, HostNeutralBackendIdentity<string>>
type MainDatabase = DiscoveredGattDatabase<string, HostNeutralBackendIdentity<string>>
type MainCharacteristicPath = CharacteristicPath<string, string, string, string, string, 'current'>
type MainDescriptorPath = DescriptorPath<string, string, string, string, string, string, 'current'>
type MainCharacteristicSnapshot = Awaited<ReturnType<MainDatabase['snapshot']>>['characteristics'][number]

const DEFAULT_DELIVERY: SubscriptionOptions['delivery'] = Object.freeze({
  itemCapacity: capacity(128),
  byteCapacity: capacity(256 * 1024),
  reservedControlCapacity: capacity(1),
  overflowPolicy: 'drop-oldest'
})
/**
 * Retention window for pre-cancelled and settled operation correlations.
 *
 * A safety bound on main-process memory, not host policy. The router must
 * remember an operation's terminal fate long enough to answer a late or
 * duplicated renderer message idempotently, and must forget it eventually or an
 * unbounded map grows for the process lifetime. Thirty seconds comfortably
 * exceeds any in-flight IPC round-trip while keeping retention bounded; renderer
 * code cannot influence it, which is the point -- it is a quota the trusted side
 * imposes on untrusted peers.
 */
const CANCELLATION_CORRELATION_TTL_MILLISECONDS = 30_000

/**
 * IPC 9 added the ordered failure-observation and platform-metadata shapes.
 * Earlier renderers can still use the 5.2.1 ordinary GATT route set, but they
 * cannot faithfully receive the ordered stream contract. Keep this at the
 * main-process boundary so a crafted legacy envelope cannot acquire an ordered
 * resource before the incompatibility is reported.
 */
const ORDERED_ROUTE_MINIMUM_IPC_PROTOCOL = 9

export interface ElectronMainBleRouterOptions {
  readonly manager: MainManager
  readonly maximumMessageBytes: number
  readonly maximumOutstandingOperations: number
  readonly maximumRetainedBytes: number
  readonly publish: (rendererClientId: string, event: ElectronBleIpcEvent) => Promise<ElectronEventDelivery>
  /**
   * Trusted Electron-main opt-in for durable recovery. The callback receives
   * only the authenticated identity established at bootstrap; renderer input
   * can never request a grant or name a recording.
   */
  readonly grantPeerFailureRecovery?: (renderer: RendererIdentity<string, string>) => boolean
  /**
   * Trusted Electron-main opt-in for scan failure recovery.  The callback
   * closes over host-selected recovery recordings; no renderer input names a
   * directory, recording, capability, or native handle.
   */
  readonly grantScanFailureRecovery?: (
    renderer: RendererIdentity<string, string>
  ) => readonly DesktopScanFailureRecovery[]
  /** Injected for deterministic expiry of lease-scoped cancellation correlation records. */
  readonly cancellationClock?: () => number
}

interface RendererResources {
  readonly securityWatches: Map<
    string,
    {
      readonly stream: BoundedAsyncStream<import('../backend-contract/security').PeerSecurityEvent>
      readonly iterator: BoundedAsyncStreamIterator<import('../backend-contract/security').PeerSecurityEvent>
      cleanup: Promise<SerializableRecord> | null
      localReturn: Promise<void> | null
      localReturned: boolean
      nativeReleased: boolean
      nativeClose: Promise<CleanupRecord> | null
    }
  >
  readonly readinessWatches: Map<
    string,
    {
      readonly connectionHandle: string
      readonly closeWatch: () => Promise<CleanupRecord>
      readonly stopIterator: () => Promise<void>
      cleanup: Promise<SerializableRecord> | null
    }
  >
  readonly parameterWatches: Map<
    string,
    {
      readonly connectionHandle: string
      readonly closeWatch: () => Promise<CleanupRecord>
      readonly stopIterator: () => Promise<void>
      cleanup: Promise<SerializableRecord> | null
    }
  >
  readonly pairingChallenges: Map<
    string,
    {
      readonly ceremonyHandle: string
      readonly kind: SecurityPairingChallenge['kind']
      readonly resolve: (value: SecurityPairingResponse) => void
      readonly reject: (error: unknown) => void
    }
  >
  readonly rendererLease: RendererLeaseIdentity
  readonly ownerKey: string
  /** Main-process-only authority for rows selected by this recovery controller. */
  readonly terminalEvidenceRecoveryLeaseId: string
  readonly scans: Map<string, ManagedScan>
  readonly connections: Map<string, MainConnection>
  readonly connectionEventSubscriptions: Map<string, ManagedConnectionEventSubscription>
  readonly databases: Map<string, ManagedDatabase>
  readonly subscriptions: Map<string, ManagedSubscription>
  readonly orderedGroups: Map<string, ManagedOrderedGroup>
  readonly releasedOrderedGroups: Map<string, SerializableRecord>
  readonly forgottenOrderedGroups: Set<string>
  readonly acquiredWriters: Map<string, ManagedAcquiredWriter>
  readonly reservedHandles: Set<string>
  readonly pendingRollbacks: Map<string, OperationAcquisitions>
  rollbackRetry: Promise<void> | null
  readonly operations: Map<string, ManagedOperation>
  readonly preCancelledOperations: Map<string, number>
  readonly settledOperations: Map<string, number>
  /**
   * Handles removed after a successful release (finding 211): the registries
   * tombstone here on every released removal, including source-terminal
   * auto-removals, so an explicit re-release reports `released` instead of
   * `ownership.denied`. Dies with the scope; never consulted across leases.
   */
  readonly releasedHandles: Set<string>
  /** Opaque, lease-bound projections; native provenance never enters IPC. */
  readonly peerFailureRecoveries: Map<string, DesktopDurablePeerNotificationFailureRecovery>
  /** True only when Electron-main granted this v10-only capability to this lease. */
  peerFailureRecoveryGranted: boolean
  /** Renderer-local aliases for process-private scan-recovery handles. */
  readonly scanFailureRecoveries: Map<string, DesktopScanFailureRecovery>
  readonly scanFailureHandles: Map<
    string,
    {
      readonly recovery: DesktopScanFailureRecovery
      readonly nativeHandle: string
      readonly kind: 'owner' | 'head' | 'cleanup'
    }
  >
  /** True only when trusted Electron-main explicitly admitted this v10 lease. */
  scanFailureRecoveryGranted: boolean
  lifecycle: 'active' | 'releasing'
  releaseResult: Promise<CleanupRecord> | null
}

interface ManagedOrphanReceipt {
  readonly record: SerializableRecord
  readonly scalars: readonly string[]
  cursor: number
  nextOffset: number | null
  page: SerializableRecord | null
  complete: boolean
}

interface ManagedOrderedGroup {
  readonly databaseHandle: string
  readonly setup: OrderedNotificationSetup
  readonly delivery: OrderedDelivery<OrderedNotificationItem> | null
  readonly openResponse: SerializableRecord
  readonly openRequest: SerializableRecord
  readonly pendingSetupObservations: OrderedNotificationItem[]
  readonly markerKey: string
  readonly acknowledgedMarkerOrdinals: Set<string>
  cleanupReleased: boolean
  cleanupResponse: SerializableRecord | null
}

interface ManagedAcquiredWriter {
  readonly databaseHandle: string
  readonly connectionHandle: string
  readonly writer: Awaited<ReturnType<MainDatabase['acquireWrite']>>
  cleanup: Promise<CleanupRecord> | null
}

interface ManagedDatabase {
  readonly connectionHandle: string
  readonly database: MainDatabase
  readonly characteristics: Map<string, MainCharacteristicPath>
  readonly descriptors: Map<string, MainDescriptorPath>
}

interface ManagedOperation {
  readonly controller: AbortController
  readonly settled: Promise<void>
  complete(): void
}

interface OperationAcquisitions {
  readonly securityWatches: Set<string>
  readonly readinessWatches: Set<string>
  readonly parameterWatches: Set<string>
  readonly scans: Set<string>
  readonly connections: Set<string>
  readonly connectionEventSubscriptions: Set<string>
  readonly databases: Set<string>
  readonly subscriptions: Set<string>
  readonly orderedGroups: Set<string>
  readonly acquiredWriters: Set<string>
}

interface ProcessOrderedOwners {
  readonly recordings: ElectronOrderedRecordingRegistry
  readonly markers: ElectronOrderedMarkerRegistry
  readonly recovery: ElectronOrderedRecordingRecovery
}

interface ElectronTerminalEvidenceEntry {
  readonly ownerKey: string
  /** Private recovery-controller authority; never derived from renderer identity. */
  recoveryLeaseId: string | null
  readonly producer: 'scan' | 'subscription'
  readonly scope: TerminalEvidenceReservationRequest['scope'] | null
  readonly source: ElectronTerminalEvidenceSource
  readonly reservation: TerminalEvidenceReservation
  ref: string | null
  digest: string | null
  byteLength: number | null
  recoverable: boolean
}

interface ElectronTerminalEvidenceSource extends TerminalEvidenceSource {
  readonly readTerminalEvidence: NonNullable<TerminalEvidenceSource['readTerminalEvidence']>
  readonly releaseTerminalEvidence: NonNullable<TerminalEvidenceSource['releaseTerminalEvidence']>
}

interface ProcessTerminalEvidenceOwners {
  readonly entries: Map<string, ElectronTerminalEvidenceEntry>
  nextId: number
}

// The backend is the process-host authority shared by all child managers.
const processOrderedOwners = new WeakMap<DesktopRustCoreBackend, ProcessOrderedOwners>()
const processTerminalEvidenceOwners = new WeakMap<object, ProcessTerminalEvidenceOwners>()
let nextRouterScope = 0n

function confirmProcessRecordingFinished(
  owners: ProcessOrderedOwners,
  recordingId: string,
  result: import('../core/continuation-recording').OrderedRecordingHostStopResult
): void {
  if (!isRecordingId(recordingId)) throw contractError('argument.invalid', 'cleanup', 'electron.ordered-recording.id')
  // Electron's desktop owner negotiates receipt revision 2 before it can
  // admit an ordered recorder.  Parsing this completion as the legacy v1
  // shape discards the negotiated failure evidence and incorrectly rejects a
  // valid finished recording.
  const recording = parseOrderedRecordingStopReceipt(result.recording, 2)
  const cleanup = parseOrderedRecordingHostCleanup(result.hostCleanup)
  if (recording.state !== 'finished' || (cleanup.state !== 'released' && cleanup.state !== 'not-applicable'))
    throw contractError('lifecycle.invalid-state', 'cleanup', 'electron.ordered-recording.not-finished')
  owners.recordings.confirmFinished(
    recordingId,
    (owner, id) => !owners.markers.hasActive(JSON.stringify([owner, 'recording', id])),
    (owner, id) => owners.recovery.forgetRecording(owner, id)
  )
}

/**
 * Canonical Electron-main router for the v4 public manager. It owns all live
 * handles; renderer messages contain only opaque IDs and copied byte values.
 */
export class ElectronMainBleRouter {
  private readonly registryScope = String(++nextRouterScope)
  private readonly manager: MainManager
  private readonly detachAttachmentListener: () => void
  /** The attachment renderers route under; main rebinds it after an adapter loss. */
  private attachment: AttachmentRecord<string>
  private publish: ElectronMainBleRouterOptions['publish']
  private readonly maximumMessageBytes: number
  private readonly maximumOutstandingOperations: number
  private readonly cancellationClock: () => number
  private readonly resources = new Map<string, RendererResources>()
  private readonly rendererOwners = new Map<string, string>()
  private readonly orderedRecordings: ElectronOrderedRecordingRegistry
  private readonly orderedMarkers: ElectronOrderedMarkerRegistry
  private readonly orderedRecovery: ElectronOrderedRecordingRecovery
  private readonly terminalEvidence: ProcessTerminalEvidenceOwners
  private readonly orphanReceipts = new Map<string, Map<string, ManagedOrphanReceipt>>()
  private orphanReservations = 0
  private readonly arbiter: ElectronMainArbiterContext<string>
  private readonly streams: ElectronRendererStreamRegistry
  private readonly connectionEvents: ElectronConnectionEventStreamRegistry
  private readonly grantPeerFailureRecovery: ((renderer: RendererIdentity<string, string>) => boolean) | undefined
  private readonly grantScanFailureRecovery:
    | ((renderer: RendererIdentity<string, string>) => readonly DesktopScanFailureRecovery[])
    | undefined
  private nextHandle = 1
  private nextEvent = 1
  private nextTerminalEvidenceRecoveryLease = 1

  constructor(options: ElectronMainBleRouterOptions) {
    this.manager = options.manager
    this.publish = options.publish
    this.maximumMessageBytes = options.maximumMessageBytes
    this.maximumOutstandingOperations = options.maximumOutstandingOperations
    this.grantPeerFailureRecovery = options.grantPeerFailureRecovery
    this.grantScanFailureRecovery = options.grantScanFailureRecovery
    const backend = this.manager.attachedBackend.backend
    let owners = backend instanceof DesktopRustCoreBackend ? processOrderedOwners.get(backend) : undefined
    if (owners === undefined) {
      owners = {
        recordings: new ElectronOrderedRecordingRegistry(options.maximumOutstandingOperations),
        markers: new ElectronOrderedMarkerRegistry(),
        recovery: new ElectronOrderedRecordingRecovery(options.maximumOutstandingOperations)
      }
      if (backend instanceof DesktopRustCoreBackend) {
        processOrderedOwners.set(backend, owners)
        const processOwners = owners
        backend.observeOrderedRecordingFinished(async (id, result) =>
          confirmProcessRecordingFinished(processOwners, id, result)
        )
      }
    }
    this.orderedRecordings = owners.recordings
    this.orderedMarkers = owners.markers
    this.orderedRecovery = owners.recovery
    const terminalOwner = this.manager.attachedBackend.backend
    const persistentTerminalOwner = terminalOwner !== null && typeof terminalOwner === 'object'
    let terminalEvidence = persistentTerminalOwner ? processTerminalEvidenceOwners.get(terminalOwner) : undefined
    if (terminalEvidence === undefined) {
      terminalEvidence = { entries: new Map(), nextId: 1 }
      if (persistentTerminalOwner) processTerminalEvidenceOwners.set(terminalOwner, terminalEvidence)
    }
    this.terminalEvidence = terminalEvidence
    this.cancellationClock = options.cancellationClock ?? (() => Date.now())
    this.streams = new ElectronRendererStreamRegistry({
      maximumMessageBytes: this.maximumMessageBytes,
      now: () =>
        typeof this.manager.monotonicNow === 'function'
          ? this.manager.monotonicNow()
          : (globalThis.performance?.now() ?? Date.now()),
      publish: (rendererLeaseId, event) => this.publish(rendererLeaseId, event),
      createEvent: (rendererLease, streamId, item) => this.event(rendererLease, streamId, item)
    })
    this.connectionEvents = new ElectronConnectionEventStreamRegistry({
      maximumMessageBytes: this.maximumMessageBytes,
      publish: (rendererLeaseId, event) => this.publish(rendererLeaseId, event),
      createEvent: (rendererLease, streamId, item) => this.event(rendererLease, streamId, item)
    })
    const attachment = this.manager.attachedBackend.attachment.attachment
    this.attachment = attachment
    const versions = createElectronHostIpcVersionAxes(this.manager.identity.versions)
    this.arbiter = new ElectronMainArbiterContext(
      {
        attachment,
        versions,
        quota: {
          maximumMessageBytes: byteLimit(options.maximumMessageBytes),
          maximumOutstandingOperations: capacity(options.maximumOutstandingOperations),
          maximumRetainedBytes: byteLimit(options.maximumRetainedBytes)
        }
      },
      {
        route: envelope => this.route(envelope),
        release: (_identity, lease) => this.releaseResources(lease.leaseId)
      }
    )
    this.detachAttachmentListener = this.manager.onAttachmentAdvanced((previous, current) =>
      this.rebindRenderers(previous, current)
    )
  }

  /**
   * IPC protocol 4: the manager followed its backend to a new attachment after
   * an adapter loss. Main (never a renderer) rebinds: later routes must name
   * the new attachment, the replaced one is refused `backend.reset` (releases
   * excepted), and every active renderer lease is told on the attachment
   * stream.
   */
  private rebindRenderers(previous: AttachmentRecord<string>, current: AttachmentRecord<string>): void {
    this.arbiter.rebindAttachment(current)
    this.attachment = current
    const item = Object.freeze({
      kind: 'value',
      value: Object.freeze({
        kind: 'backend-restarted',
        schemaVersion: 1,
        previousAttachmentId: String(previous.attachmentId),
        attachmentId: String(current.attachmentId),
        attachment: ipcAttachmentRecordV2(current)
      })
    })
    for (const resources of this.resources.values()) {
      resources.peerFailureRecoveries.clear()
      resources.scanFailureRecoveries.clear()
      resources.scanFailureHandles.clear()
      if (resources.lifecycle !== 'active') continue
      const event = this.event(resources.rendererLease, IPC_ATTACHMENT_STREAM_ID, item)
      this.publish(String(resources.rendererLease.leaseId), event).then(
        delivery => {
          if (delivery !== 'delivered') {
            console.error('[ElectronMainBleRouter] Attachment rebind was not delivered:', {
              rendererLease: String(resources.rendererLease.leaseId),
              delivery
            })
          }
        },
        error => {
          console.error('[ElectronMainBleRouter] Attachment rebind delivery failed:', error)
        }
      )
    }
  }

  async dispatch<Renderer extends string, Operation extends string>(
    sender: TrustedIpcSender<string, Renderer>,
    request: ElectronBleIpcRequest<string, Renderer, Operation>
  ): Promise<ElectronBleIpcSuccessResponse<string, Renderer>> {
    if (request.kind === 'bootstrap') {
      const renderer = rendererIdentity(sender)
      const versions = createElectronIpcVersionAxes(this.manager.identity.versions, request.offer)
      const rendererLease = this.arbiter.registerRenderer(renderer, versions, sender.securityPermissions)
      this.rendererOwners.set(
        String(rendererLease.leaseId),
        JSON.stringify([String(renderer.clientId), renderer.windowScope, renderer.sessionScope])
      )
      const resources = this.resourcesFor(rendererLease)
      await this.hydrateTerminalEvidenceRecovery(resources)
      if (versions.ipcProtocol.selected.value >= 10 && this.grantPeerFailureRecovery?.(renderer) === true) {
        this.grantDurablePeerFailureRecovery(resources)
        resources.peerFailureRecoveryGranted = true
      }
      if (versions.ipcProtocol.selected.value >= 10 && this.grantScanFailureRecovery !== undefined) {
        const scanRecoveries = this.grantScanFailureRecovery(renderer)
        resources.scanFailureRecoveryGranted = true
        for (const recovery of scanRecoveries) {
          resources.scanFailureRecoveries.set(`scan-failure-recovery-${this.nextHandle++}`, recovery)
        }
      }
      return {
        kind: 'bootstrap',
        bootstrap: this.bootstrap(renderer, rendererLease, versions)
      }
    }
    if (request.kind === 'route') {
      const payload = await this.arbiter.route(sender, request.envelope)
      return { kind: 'route', payload }
    }
    if (request.kind === 'release') {
      const cleanup = await this.arbiter.releaseRenderer(sender, request.rendererLease)
      return { kind: 'release', cleanup }
    }
    throw contractError('protocol.violation', 'ipc', 'electron-main-router.event-ack-binding-required')
  }

  /** Checks compatibility without allocating a lease or touching host resources. */
  validateBootstrapOffer(offer: IpcCompatibilityOffer): void {
    createElectronIpcVersionAxes(this.manager.identity.versions, offer)
  }

  /** Enforces the configured byte limit before any request reaches routing or acknowledgement state. */
  validateRequest<Renderer extends string, Operation extends string>(
    request: ElectronBleIpcRequest<string, Renderer, Operation>
  ): void {
    let byteLength: number
    try {
      byteLength = electronRequestByteLength(request)
    } catch (error) {
      if (error instanceof BackendContractError) {
        throw error
      }
      throw contractError('protocol.malformed', 'ipc', 'electron-main-router.request-shape')
    }
    if (byteLength > this.maximumMessageBytes) {
      throw contractError('bytes.too-large', 'ipc', 'electron-main-router.request-size')
    }
  }

  /** Releases the authenticated renderer after a host-owned WebContents lifetime event. */
  releaseRenderer<Renderer extends string>(
    sender: TrustedIpcSender<string, Renderer>,
    rendererLease: RendererLeaseIdentity
  ): Promise<CleanupRecord> {
    return this.arbiter.releaseRenderer(sender, rendererLease)
  }

  /**
   * Stops one stream after its authenticated renderer can no longer accept
   * events. The owning binding calls this only for its own renderer identity.
   */
  async terminateStream(
    rendererLease: RendererLeaseIdentity,
    streamId: string,
    reason: 'renderer-backpressure' | 'renderer-unavailable'
  ): Promise<void> {
    const rendererLeaseId = String(rendererLease.leaseId)
    const resources = this.resources.get(rendererLeaseId)
    if (resources === undefined) {
      return
    }
    if (await this.connectionEvents.terminate(resources, rendererLease, streamId, reason)) {
      return
    }
    await this.streams.terminate(resources, rendererLease, streamId, reason)
  }

  async destroy(): Promise<CleanupRecord> {
    this.detachAttachmentListener()
    const rendererFailures: CleanupFailure[] = []
    for (const clientId of [...this.resources.keys()]) {
      try {
        const cleanup = await this.releaseResources(clientId)
        rendererFailures.push(...cleanup.failures)
      } catch (error) {
        console.error('[ElectronMainBleRouter] Renderer cleanup rejected during router destroy:', { clientId, error })
        rendererFailures.push({
          resourceKind: 'electron-renderer',
          error: normalizedCleanupError(error)
        })
      }
    }
    let managerCleanup: CleanupRecord
    try {
      managerCleanup = await this.manager.destroy()
    } catch (error) {
      console.error('[ElectronMainBleRouter] Manager cleanup rejected during router destroy:', error)
      managerCleanup = {
        state: 'release-failed',
        failures: [{ resourceKind: 'manager', error: normalizedCleanupError(error) }]
      }
    }
    const failures = [...rendererFailures, ...managerCleanup.failures]
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  /** Installs the application-owned event delivery binding after IPC authentication is configured. */
  setEventPublisher(publish: ElectronMainBleRouterOptions['publish']): void {
    this.publish = publish
  }

  /**
   * Announces a v10 recovery grant only after the binding has registered the
   * authenticated renderer that can receive the attachment event. Bootstrap
   * cannot publish this itself: its dispatch happens before binding ownership
   * is installed, which would terminalize a valid lease as unavailable.
   */
  async publishPeerFailureRecoveryAvailability(rendererLease: RendererLeaseIdentity): Promise<ElectronEventDelivery> {
    const resources = this.resources.get(String(rendererLease.leaseId))
    if (
      resources === undefined ||
      resources.lifecycle !== 'active' ||
      String(resources.rendererLease.generation) !== String(rendererLease.generation)
    ) {
      return 'terminalized'
    }
    if (!resources.peerFailureRecoveryGranted) return 'delivered'
    return this.publish(
      String(rendererLease.leaseId),
      this.event(
        rendererLease,
        IPC_ATTACHMENT_STREAM_ID,
        Object.freeze({
          kind: 'value',
          value: Object.freeze({
            kind: 'diagnostic-warning',
            schemaVersion: 1,
            code: 'gatt.peer-failure-recovery-available'
          })
        })
      )
    )
  }

  private bootstrap<Renderer extends string>(
    renderer: RendererIdentity<string, Renderer>,
    rendererLease: RendererLeaseIdentity,
    versions: IpcVersionAxes
  ): ElectronRendererBootstrap<string, Renderer> {
    const attachment = this.attachment
    const capabilities = this.manager.capabilities()
    return Object.freeze({
      attachment,
      attachmentId: attachment.attachmentId,
      versions,
      capabilities: snapshotCapabilityDescriptors(capabilities, String(attachment.backendGeneration)),
      discovery: discoveryDescriptor(capabilities),
      renderer,
      rendererLease
    })
  }

  private async route<Renderer extends string, Operation extends string>(
    received: IpcEnvelope<string, Renderer, Operation>
  ): Promise<SerializableRecord> {
    let receivedAt: number | null = null
    const receiptClock = (): number => (receivedAt ??= this.manager.monotonicNow())
    const envelope = { ...received, payload: admitRelativeBudget(received.payload, receiptClock) }
    this.assertOrderedRouteProtocol(envelope)
    const resources = this.resourcesFor(envelope.rendererLease)
    if (resources.lifecycle !== 'active') {
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron-main-router.renderer-releasing')
    }
    if (envelope.command === 'operation.cancel') {
      return this.cancel(resources, envelope.payload)
    }
    this.retryOperationRollbacks(resources).catch(error => {
      console.error('[ElectronMainBleRouter] Scoped rollback retry failed:', { error })
    })
    if (resources.lifecycle !== 'active') {
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron-main-router.renderer-releasing')
    }
    const operationKey = String(envelope.correlation)
    this.pruneCancellationCorrelations(resources)
    if (resources.operations.has(operationKey) || resources.pendingRollbacks.has(operationKey)) {
      throw contractError('protocol.violation', 'ipc', 'electron-main-router.correlation-in-flight')
    }
    const controller = new AbortController()
    const operation = createManagedOperation(controller)
    const acquisitions = createOperationAcquisitions()
    if (
      isResourceAcquisitionCommand(envelope.command) &&
      resources.pendingRollbacks.size >= this.maximumOutstandingOperations
    ) {
      throw contractError('stream.quota', 'ipc', 'electron-main-router.pending-rollback-capacity')
    }
    let compensationAttempted = false
    let admitted = false
    resources.settledOperations.delete(operationKey)
    if (resources.preCancelledOperations.delete(operationKey)) {
      controller.abort()
    }
    resources.operations.set(operationKey, operation)
    try {
      const preAdmissionFailure = operationAdmissionFailure(
        controller,
        envelope.payload,
        receiptClock,
        envelope.command
      )
      if (preAdmissionFailure !== null) {
        throw preAdmissionFailure
      }
      admitted = true
      let response: SerializableRecord
      if (envelope.command === 'peerFailureRecovery.list') {
        this.assertPeerFailureRecoveryProtocol(envelope)
        response = this.listPeerFailureRecovery(resources)
      } else if (envelope.command === 'peerFailureRecovery.read') {
        this.assertPeerFailureRecoveryProtocol(envelope)
        response = await this.readPeerFailureRecovery(resources, envelope.payload)
      } else if (envelope.command === 'peerFailureRecovery.ack') {
        this.assertPeerFailureRecoveryProtocol(envelope)
        response = await this.acknowledgePeerFailureRecovery(resources, envelope.payload)
      } else if (envelope.command.startsWith('scanFailureRecovery.')) {
        this.assertScanFailureRecoveryProtocol(envelope)
        response = await this.routeScanFailureRecovery(resources, envelope.command, envelope.payload)
      } else if (envelope.command.startsWith('terminalEvidence.')) {
        this.assertTerminalEvidenceProtocol(envelope)
        response = await this.routeTerminalEvidence(
          resources,
          envelope.command,
          envelope.payload,
          envelope.binaryPayload
        )
      } else if (envelope.command === 'scan.start') {
        response = await this.startScan(resources, envelope, controller, acquisitions)
      } else if (envelope.command === 'scan.stop') {
        response = await this.stopScan(resources, envelope.payload)
      } else if (envelope.command === 'connection.connect') {
        response = await this.connect(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'adapter.state') {
        response = await this.adapterState(controller)
      } else if (envelope.command === 'security.watch.subscribe') {
        response = this.watchSecurity(resources, envelope.payload, acquisitions)
      } else if (envelope.command === 'security.watch.unsubscribe') {
        response = await this.unwatchSecurity(resources, requiredString(envelope.payload, 'handle'))
      } else if (envelope.command === 'security.custom-ceremony') {
        const challengeId = requiredString(envelope.payload, 'challengeId')
        const challenge = resources.pairingChallenges.get(challengeId)
        if (challenge === undefined || challenge.ceremonyHandle !== envelope.payload.ceremonyHandle)
          throw contractError('ownership.denied', 'ipc', 'electron.security.challenge-owner')
        const result = pairingResponse(envelope.payload.response)
        if (result.kind !== challenge.kind)
          throw contractError('protocol.violation', 'ipc', 'electron.security.challenge-kind')
        resources.pairingChallenges.delete(challengeId)
        challenge.resolve(result)
        response = { accepted: true }
      } else if (
        ['security.state', 'security.pair', 'security.cancel-pairing', 'security.unpair'].includes(envelope.command)
      ) {
        response = await this.security(resources, envelope.command, envelope.payload, controller)
      } else if (
        [
          'peers.resolve',
          'peers.known',
          'peers.connected',
          'peers.bonded',
          'peers.authorized',
          'peers.restored'
        ].includes(envelope.command)
      ) {
        response = await this.peerDirectory(envelope.command, envelope.payload, controller)
      } else if (envelope.command === 'connection.rssi') {
        response = await this.readRssi(resources, envelope.payload, controller)
      } else if (envelope.command === 'connection.phy') {
        const connection = requiredResource(
          resources.connections,
          requiredString(envelope.payload, 'connectionHandle'),
          'connection'
        )
        const observation = await connection.readPhy(operationOptions(envelope.payload, controller))
        response = Object.freeze({
          connectionId: connection.connectionId,
          connectionGeneration: connection.connectionGeneration,
          txPhy: observation.txPhy,
          rxPhy: observation.rxPhy,
          observedAtMonotonicMs: observation.observedAtMonotonicMs
        })
      } else if (envelope.command === 'connection.request-priority') {
        const connection = requiredResource(
          resources.connections,
          requiredString(envelope.payload, 'connectionHandle'),
          'connection'
        )
        const priority = requiredString(envelope.payload, 'priority')
        if (priority !== 'balanced' && priority !== 'low-power' && priority !== 'high-throughput')
          throw contractError('argument.invalid', 'connection', 'electron.request-priority')
        const result = await connection.requestPriority(priority, operationOptions(envelope.payload, controller))
        response = Object.freeze({
          accepted: result.accepted,
          connectionId: connection.connectionId,
          connectionGeneration: connection.connectionGeneration
        })
      } else if (envelope.command === 'connection.effective-mtu') {
        response = await this.effectiveMtu(resources, envelope.payload, controller)
      } else if (envelope.command === 'connection.maximum-write-length') {
        response = await this.maximumWriteLength(resources, envelope.payload, controller)
      } else if (envelope.command === 'connection.write-readiness.subscribe') {
        response = await this.subscribeWriteReadiness(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'connection.write-readiness.unsubscribe') {
        response = await this.unsubscribeWriteReadiness(
          resources,
          requiredString(envelope.payload, 'writeReadinessHandle')
        )
      } else if (envelope.command === 'connection.parameters') {
        response = await this.readConnectionParameters(resources, envelope.payload, controller)
      } else if (envelope.command === 'connection.parameters.subscribe') {
        response = await this.subscribeConnectionParameters(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'connection.parameters.unsubscribe') {
        response = await this.unsubscribeConnectionParameters(
          resources,
          requiredString(envelope.payload, 'parameterEventsHandle')
        )
      } else if (envelope.command === 'connection.disconnect') {
        response = await this.disconnect(resources, envelope.payload)
      } else if (envelope.command === 'connection.events.subscribe') {
        response = this.subscribeConnectionEvents(resources, envelope.payload, acquisitions)
      } else if (envelope.command === 'connection.events.ready') {
        response = this.readyConnectionEvents(resources, envelope.payload)
      } else if (envelope.command === 'connection.events.unsubscribe') {
        response = await this.unsubscribeConnectionEvents(resources, envelope.payload)
      } else if (envelope.command === 'gatt.discover') {
        response = await this.discover(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'gatt.database.release') {
        response = await this.releaseDatabase(resources, envelope.payload)
      } else if (envelope.command === 'gatt.read') {
        response = await this.read(resources, envelope.payload, controller)
      } else if (envelope.command === 'gatt.write-when-ready') {
        response = await this.write(resources, envelope.payload, envelope.binaryPayload, controller, true)
      } else if (envelope.command === 'gatt.write') {
        response = await this.write(resources, envelope.payload, envelope.binaryPayload, controller)
      } else if (envelope.command === 'gatt.descriptor.read') {
        response = await this.readDescriptor(resources, envelope.payload, controller)
      } else if (envelope.command === 'gatt.descriptor.write') {
        response = await this.writeDescriptor(resources, envelope.payload, envelope.binaryPayload, controller)
      } else if (envelope.command === 'gatt.acquire-write') {
        response = await this.acquireWriter(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'gatt.acquired-write') {
        response = await this.writeAcquired(resources, envelope.payload, envelope.binaryPayload, controller)
      } else if (envelope.command === 'gatt.acquired-write.close') {
        response = cleanupRecord(
          await this.closeAcquiredWriter(resources, requiredString(envelope.payload, 'acquiredHandle'))
        )
      } else if (envelope.command === 'gatt.ordered-recording.recovery') {
        const startHandle = requiredString(envelope.payload, 'startHandle')
        const recovery = this.orderedRecordings.recovery(resources.ownerKey, startHandle)
        response = this.orderedRecovery.issue(resources.ownerKey, startHandle, recovery.peer, () => recovery.handle)
      } else if (envelope.command === 'gatt.ordered-recording.reclaim.pending') {
        const after = envelope.payload.after
        const limit = envelope.payload.limit
        if (after !== undefined && after !== null && typeof after !== 'string')
          throw contractError('protocol.malformed', 'ipc', 'electron.recording-recovery.cursor')
        if (limit !== undefined && typeof limit !== 'number')
          throw contractError('protocol.malformed', 'ipc', 'electron.recording-recovery.limit')
        let pageLimit = limit ?? 64
        response = this.orderedRecovery.pending(resources.ownerKey, after ?? null, pageLimit)
        while (snapshotSerializableRecord(response).byteLength > this.maximumMessageBytes && pageLimit > 1) {
          pageLimit = Math.max(1, Math.floor(pageLimit / 2))
          response = this.orderedRecovery.pending(resources.ownerKey, after ?? null, pageLimit)
        }
      } else if (envelope.command === 'gatt.ordered-recording.reclaim.reserve') {
        const database = this.database(resources, envelope.payload)
        const connection = requiredResource(resources.connections, database.connectionHandle, 'connection')
        const reserve = database.database.reserveOrderedRecordingReclaim
        if (!reserve) throw contractError('capability.unsupported', 'gatt', 'electron.recording-recovery')
        response = this.orderedRecovery.reserve(
          resources.ownerKey,
          requiredString(envelope.payload, 'recoveryToken'),
          requiredString(envelope.payload, 'callerKey'),
          requiredString(envelope.payload, 'databaseHandle'),
          this.recordingLease(resources),
          String(connection.peerId),
          handle => reserve.call(database.database, handle)
        )
      } else if (envelope.command.startsWith('gatt.ordered-recording.reclaim.')) {
        const action = envelope.command.slice('gatt.ordered-recording.reclaim.'.length)
        const id = requiredString(envelope.payload, 'reclaimHandle')
        const lease = this.recordingLease(resources)
        if (action === 'ack-retirement') response = this.orderedRecovery.acknowledge(resources.ownerKey, id, lease)
        else if (action === 'forget-retirement') response = this.orderedRecovery.forget(resources.ownerKey, id, lease)
        else if (action === 'confirm-forgotten')
          response = this.orderedRecovery.confirmForgotten(resources.ownerKey, id, lease)
        else if (
          action === 'start' ||
          action === 'status' ||
          action === 'commit' ||
          action === 'cancel' ||
          action === 'release' ||
          action === 'retire'
        )
          response = await this.orderedRecovery.action(resources.ownerKey, id, lease, action)
        else throw contractError('protocol.malformed', 'ipc', 'electron.recording-recovery.action')
      } else if (envelope.command === 'gatt.ordered-recording.marker-owner') {
        const database = this.database(resources, envelope.payload)
        const connection = requiredResource(resources.connections, database.connectionHandle, 'connection')
        if (!database.database.reserveOrderedRecordingMarker)
          throw contractError('capability.unsupported', 'gatt', 'electron.ordered-recording.marker')
        const startHandle = this.orderedRecovery.markerOwner(
          resources.ownerKey,
          requiredString(envelope.payload, 'reclaimHandle'),
          requiredString(envelope.payload, 'databaseHandle'),
          this.recordingLease(resources)
        )
        const recordingId = this.orderedRecordings.markerRecordingId(
          resources.ownerKey,
          startHandle,
          String(connection.peerId)
        )
        if (recordingId !== requiredString(envelope.payload, 'recordingId'))
          throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.recording')
        response = { startHandle }
      } else if (envelope.command === 'gatt.ordered-recording.markers') {
        const startHandle = requiredString(envelope.payload, 'startHandle')
        this.orderedRecordings.assertOwned(resources.ownerKey, startHandle)
        response = await this.orderedMarkers.list(JSON.stringify([resources.ownerKey, 'recording', startHandle]))
      } else if (
        envelope.command.startsWith('gatt.ordered.marker.') ||
        envelope.command.startsWith('gatt.ordered-recording.marker.')
      ) {
        response = await this.orderedMarker(
          resources,
          envelope.command,
          envelope.payload,
          envelope.binaryPayload,
          controller
        )
      } else if (envelope.command === 'gatt.ordered-recording.reserve') {
        response = this.orderedRecordings.reserve(
          resources.ownerKey,
          this.recordingLease(resources),
          requiredString(envelope.payload, 'reservationToken')
        )
      } else if (envelope.command === 'gatt.ordered-recording.starts') {
        const after = envelope.payload.after
        const limit = envelope.payload.limit
        if (after !== undefined && after !== null && typeof after !== 'string')
          throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.after')
        if (limit !== undefined && typeof limit !== 'number')
          throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.limit')
        let pageLimit = limit ?? 64
        response = this.orderedRecordings.list(resources.ownerKey, after ?? null, pageLimit)
        while (snapshotSerializableRecord(response).byteLength > this.maximumMessageBytes && pageLimit > 1) {
          pageLimit = Math.max(1, Math.floor(pageLimit / 2))
          response = this.orderedRecordings.list(resources.ownerKey, after ?? null, pageLimit)
        }
      } else if (envelope.command === 'gatt.ordered-recording.status') {
        response = await this.orderedRecordings.status(
          resources.ownerKey,
          requiredString(envelope.payload, 'startHandle')
        )
      } else if (envelope.command === 'gatt.ordered-recording.retire') {
        const startHandle = requiredString(envelope.payload, 'startHandle')
        const markerKey = JSON.stringify([resources.ownerKey, 'recording', startHandle])
        response = await this.orderedRecordings.retire(
          resources.ownerKey,
          startHandle,
          !this.orderedMarkers.hasActive(markerKey)
        )
        if (!this.orderedRecordings.has(resources.ownerKey, startHandle)) this.orderedMarkers.releaseOwner(markerKey)
      } else if (envelope.command === 'gatt.ordered-recording.start') {
        response = await this.startOrderedRecording(resources, envelope.payload, controller)
      } else if (envelope.command === 'gatt.ordered.open') {
        response = await this.openOrdered(resources, envelope.payload, controller, acquisitions)
      } else if (envelope.command === 'gatt.ordered.orphans') {
        response = this.inspectOrderedOrphans(resources.ownerKey)
      } else if (envelope.command === 'gatt.ordered.orphan-page') {
        response = this.pageOrderedOrphan(resources.ownerKey, envelope.payload)
      } else if (envelope.command === 'gatt.ordered.orphan-ack') {
        response = this.acknowledgeOrderedOrphan(resources.ownerKey, requiredString(envelope.payload, 'receiptId'))
      } else if (envelope.command === 'gatt.ordered.status') {
        response = this.statusOrdered(resources, envelope.payload)
      } else if (envelope.command === 'gatt.ordered.forget') {
        response = this.forgetOrdered(resources, envelope.payload)
      } else if (envelope.command === 'gatt.ordered.retire') {
        response = this.retireOrdered(resources, envelope.payload)
      } else if (envelope.command === 'gatt.ordered.next') {
        response = await this.nextOrdered(resources, envelope.payload)
      } else if (envelope.command === 'gatt.ordered.close') {
        response = await this.closeOrdered(resources, envelope.payload)
      } else if (envelope.command === 'gatt.acquire-notifications') {
        response = await this.subscribe(resources, envelope, controller, acquisitions, true)
      } else if (envelope.command === 'gatt.subscribe') {
        response = await this.subscribe(resources, envelope, controller, acquisitions)
      } else if (envelope.command === 'gatt.unsubscribe') {
        response = await this.unsubscribe(resources, envelope.payload)
      } else {
        throw contractError('argument.invalid', 'ipc', 'electron-main-router.command')
      }
      if (!reportsCompletedEffect(envelope.command)) {
        const admissionFailure = operationAdmissionFailure(
          controller,
          envelope.payload,
          () => this.manager.monotonicNow(),
          envelope.command
        )
        if (admissionFailure !== null) {
          compensationAttempted = true
          const rollback = await this.rollbackOperationAdmission(resources, acquisitions, envelope)
          if (rollback.state === 'release-failed') {
            resources.pendingRollbacks.set(operationKey, acquisitions)
            throw contractError('lifecycle.invalid-state', 'ipc', 'electron-main-router.rollback-release-required')
          }
          throw admissionFailure
        }
      }
      if (snapshotSerializableRecord(response).byteLength > this.maximumMessageBytes) {
        if (!isDestructiveCleanupCommand(envelope.command)) {
          compensationAttempted = true
          const rollback = await this.rollbackOperationAdmission(resources, acquisitions, envelope)
          if (rollback.state === 'release-failed') {
            resources.pendingRollbacks.set(operationKey, acquisitions)
            throw contractError('lifecycle.invalid-state', 'ipc', 'electron-main-router.rollback-release-required')
          }
        }
        throw contractError('bytes.too-large', 'ipc', 'electron-main-router.response-size')
      }
      return response
    } catch (error) {
      if (admitted && !compensationAttempted) {
        const rollback = await this.rollbackOperationAdmission(resources, acquisitions, envelope)
        if (rollback.state === 'release-failed') {
          resources.pendingRollbacks.set(operationKey, acquisitions)
          throw contractError('lifecycle.invalid-state', 'ipc', 'electron-main-router.rollback-release-required')
        }
      }
      throw error
    } finally {
      if (resources.operations.get(operationKey) === operation) {
        resources.operations.delete(operationKey)
      }
      operation.complete()
      this.recordSettledOperation(resources, operationKey)
    }
  }

  private async startScan<Renderer extends string, Operation extends string>(
    resources: RendererResources,
    envelope: IpcEnvelope<string, Renderer, Operation>,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const query = decodeIpcScanQuery(envelope.payload.query, 'electron-main-router.scan-query')
    if (typeof this.manager.planScan !== 'function') {
      throw contractError('capability.unavailable', 'scan', 'electron-main-router.scan-planner')
    }
    let plan: ScanPlan
    try {
      const planned = this.manager.planScan(query)
      if (planned === null) {
        throw contractError('capability.unavailable', 'scan', 'electron-main-router.scan-planner')
      }
      plan = snapshotScanPlan(planned)
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      throw contractError('protocol.violation', 'scan', 'electron-main-router.scan-plan')
    }
    if (plan.queryDigest !== query.digest || plan.sourceQuery.digest !== query.digest) {
      throw contractError('protocol.violation', 'scan', 'electron-main-router.scan-plan-digest')
    }
    const scan = await this.manager.scan({
      ...(envelope.payload.platform === undefined
        ? {}
        : { platform: decodeIpcScanPlatform(envelope.payload.platform) }),
      query,
      plan,
      filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
      duplicatePolicy: 'all',
      timestampPolicy: 'receipt-monotonic',
      delivery: deliveryFromPayload(envelope.payload),
      deadline: deadlineFromPayload(envelope.payload),
      signal: controller.signal,
      sharing: { mode: 'owner', allowSharing: false }
    })
    const handle = this.allocateHandle('scan')
    this.streams.registerScan(resources, envelope.rendererLease, handle, scan)
    acquisitions.scans.add(handle)
    return Object.freeze({
      handle,
      backendGeneration: String(this.attachment.backendGeneration),
      plan: encodeIpcScanPlan(plan)
    })
  }

  private async connect(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const address = payload.address
    let peerId
    if (address !== undefined) {
      if (
        typeof address !== 'object' ||
        address === null ||
        Array.isArray(address) ||
        !('address' in address) ||
        !('addressType' in address) ||
        typeof address.address !== 'string' ||
        !/^(?:[0-9A-F]{2}:){5}[0-9A-F]{2}$/.test(address.address) ||
        (address.addressType !== 'public' && address.addressType !== 'random') ||
        payload.peerId !== undefined
      )
        throw contractError('argument.invalid', 'connection', 'electron.connect.address')
      const connections = this.manager.attachedBackend.backend.connections
      if (!this.manager.supports('peer:address-targeting') || connections.peerFromAddress === undefined)
        throw contractError('capability.unsupported', 'connection', 'electron.connect.address')
      peerId = connections.peerFromAddress({ address: address.address, addressType: address.addressType })
    } else peerId = opaqueId(requiredString(payload, 'peerId'), 'peer', 'electron-router')
    const intent = payload.intent
    if (intent !== undefined && intent !== 'direct' && intent !== 'when-available')
      throw contractError('argument.invalid', 'connection', 'electron.connect.intent')
    const transport = payload.transport
    if (transport !== undefined && transport !== 'le' && transport !== 'auto')
      throw contractError('argument.invalid', 'connection', 'electron.connect.transport')
    const preferredPhy = decodePreferredPhy(payload.preferredPhy)
    const connection = await this.manager.connect(peerId, {
      ...operationOptions(payload, controller),
      ...(intent === undefined ? {} : { intent }),
      ...(transport === undefined ? {} : { transport }),
      ...(preferredPhy === undefined ? {} : { preferredPhy })
    })
    const handle = this.allocateHandle('connection')
    resources.connections.set(handle, connection)
    acquisitions.connections.add(handle)
    return Object.freeze({
      handle,
      peerId: String(connection.peerId),
      connectionId: String(connection.connectionId),
      ownerLeaseId: String(resources.rendererLease.leaseId),
      connectionGeneration: String(connection.connectionGeneration)
    })
  }

  private async security(
    resources: RendererResources,
    command: string,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const backend = this.manager.securityBackend()
    if (backend === undefined) throw contractError('capability.unsupported', 'platform', `electron.${command}`)
    const peerId = requiredString(payload, 'peerId')
    const options = operationOptions(payload, controller)
    if (command === 'security.state') {
      const state = await backend.state(peerId, options)
      return { state: { ...state, limitations: state.limitations.map(value => ({ ...value })) } }
    }
    if (command === 'security.cancel-pairing') return { result: { ...(await backend.cancelPairing(peerId, options)) } }
    if (command === 'security.unpair') return { result: { ...(await backend.unpair(peerId, options)) } }
    const transport = payload.transport
    const protection = payload.protection
    const secureConnections = payload.secureConnections
    if (
      (transport !== 'auto' && transport !== 'le') ||
      (protection !== 'system-default' && protection !== 'encrypted' && protection !== 'authenticated') ||
      (secureConnections !== 'prefer' && secureConnections !== 'require' && secureConnections !== 'disallow') ||
      (payload.ceremony !== 'system' && payload.ceremony !== 'agent')
    )
      throw contractError('argument.invalid', 'platform', 'electron.security.pair-options')
    const ceremonyHandle = payload.ceremony === 'system' ? null : requiredString(payload, 'ceremonyHandle')
    if (ceremonyHandle !== null && !/^security-ceremony-[1-9][0-9]*$/.test(ceremonyHandle))
      throw contractError('argument.invalid', 'ipc', 'electron.security.ceremony-handle')
    let result
    try {
      result = await backend.pair(peerId, {
        ...options,
        transport,
        protection,
        secureConnections,
        ceremony:
          ceremonyHandle === null
            ? 'system'
            : {
                kind: 'agent',
                agent: {
                  onChallenge: challenge => {
                    if (challenge.peerId !== peerId)
                      throw contractError('protocol.violation', 'ipc', 'electron.security.challenge-peer')
                    return this.challenge(resources, ceremonyHandle, challenge, controller)
                  }
                }
              }
      })
    } finally {
      if (ceremonyHandle !== null)
        for (const challenge of resources.pairingChallenges.values()) {
          if (challenge.ceremonyHandle === ceremonyHandle)
            challenge.reject(contractError('operation.aborted', 'platform', 'electron.security.ceremony-retired'))
        }
    }
    if ('state' in result)
      return {
        result: {
          outcome: result.outcome,
          state: { ...result.state, limitations: result.state.limitations.map(value => ({ ...value })) }
        }
      }
    return { result: { ...result } }
  }

  private challenge(
    resources: RendererResources,
    ceremonyHandle: string,
    challenge: SecurityPairingChallenge,
    controller: AbortController
  ): Promise<SecurityPairingResponse> {
    if (resources.pairingChallenges.has(challenge.challengeId))
      throw contractError('protocol.violation', 'ipc', 'electron.security.challenge-identity')
    return new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => finish(contractError('operation.timed-out', 'platform', 'electron.security.challenge')),
        Math.max(0, challenge.deadlineMonotonicMs - this.manager.monotonicNow())
      )
      const abort = (): void => finish(contractError('operation.aborted', 'platform', 'electron.security.challenge'))
      const finish = (error?: unknown, value?: SecurityPairingResponse): void => {
        clearTimeout(timer)
        controller.signal.removeEventListener('abort', abort)
        resources.pairingChallenges.delete(challenge.challengeId)
        if (error !== undefined) reject(error)
        else if (value !== undefined) resolve(value)
      }
      resources.pairingChallenges.set(challenge.challengeId, {
        ceremonyHandle,
        kind: challenge.kind,
        resolve: value => finish(undefined, value),
        reject: error => finish(error)
      })
      controller.signal.addEventListener('abort', abort, { once: true })
      if (controller.signal.aborted) {
        abort()
        return
      }
      const { deadlineMonotonicMs, ...fields } = challenge
      this.publish(
        String(resources.rendererLease.leaseId),
        this.event(resources.rendererLease, ceremonyHandle, {
          kind: 'value',
          value: { ...fields, budgetMs: Math.max(0, deadlineMonotonicMs - this.manager.monotonicNow()) }
        })
      ).then(
        delivery => {
          if (delivery === 'terminalized')
            finish(contractError('platform.transport', 'ipc', 'electron.security.challenge'))
        },
        error => finish(error)
      )
    })
  }

  private watchSecurity(
    resources: RendererResources,
    payload: SerializableRecord,
    acquisitions: OperationAcquisitions
  ): SerializableRecord {
    const backend = this.manager.securityBackend()
    if (backend === undefined) throw contractError('capability.unsupported', 'platform', 'electron.security.watch')
    const peerId = requiredString(payload, 'peerId')
    const stream = backend.watch(peerId)
    const iterator = stream[Symbol.asyncIterator]()
    const handle = this.allocateHandle('subscription')
    resources.securityWatches.set(handle, {
      stream,
      iterator,
      cleanup: null,
      localReturn: null,
      localReturned: false,
      nativeReleased: false,
      nativeClose: null
    })
    acquisitions.securityWatches.add(handle)
    const pump = async (): Promise<void> => {
      while (resources.securityWatches.has(handle)) {
        const item = await iterator.next()
        if (item.done) break
        if (item.value.kind === 'value' && (!isSecurityEvent(item.value.value) || item.value.value.peerId !== peerId))
          throw contractError('protocol.violation', 'ipc', 'electron.security.watch-event')
        const value =
          item.value.kind === 'value'
            ? {
                ...item.value,
                value: {
                  ...item.value.value,
                  state: {
                    ...item.value.value.state,
                    limitations: item.value.value.state.limitations.map(entry => ({ ...entry }))
                  }
                }
              }
            : item.value.kind === 'terminal'
              ? { ...item.value, error: item.value.error == null ? null : serializeNormalizedError(item.value.error) }
              : { ...item.value }
        const delivery = await this.publish(
          String(resources.rendererLease.leaseId),
          this.event(resources.rendererLease, handle, value)
        )
        if (delivery === 'terminalized' || item.value.kind === 'terminal') break
      }
      await this.unwatchSecurity(resources, handle)
    }
    pump().catch(async error => {
      console.error('[ElectronMainBleRouter] Security watch failed:', error)
      try {
        await this.publish(
          String(resources.rendererLease.leaseId),
          this.event(resources.rendererLease, handle, {
            kind: 'terminal',
            reason: 'source-failed',
            droppedItems: 0,
            droppedBytes: 0,
            replacedItems: 0,
            error: serializeNormalizedError(normalizedCleanupError(error))
          })
        )
      } catch (deliveryError) {
        console.error('[ElectronMainBleRouter] Security watch failure delivery failed:', deliveryError)
      }
      try {
        await this.unwatchSecurity(resources, handle)
      } catch (cleanupError) {
        console.error('[ElectronMainBleRouter] Security watch cleanup failed:', cleanupError)
      }
    })
    return { handle }
  }

  private async subscribeWriteReadiness(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const connectionHandle = requiredString(payload, 'connectionHandle')
    const handle = requiredString(payload, 'writeReadinessHandle')
    const connection = requiredResource(resources.connections, connectionHandle, 'connection')
    if (
      resources.readinessWatches.has(handle) ||
      resources.releasedHandles.has(handle) ||
      resources.reservedHandles.has(handle)
    ) {
      throw contractError('protocol.violation', 'ipc', 'electron.write-readiness.duplicate')
    }
    resources.reservedHandles.add(handle)
    try {
      const opened = await connection.writeWithoutResponseReadiness(operationOptions(payload, controller))
      let stopIterator = async (): Promise<void> => undefined
      resources.readinessWatches.set(handle, {
        connectionHandle,
        closeWatch: () => opened.close(),
        stopIterator: () => stopIterator(),
        cleanup: null
      })
      acquisitions.readinessWatches.add(handle)
      const iterator = opened.events[Symbol.asyncIterator]()
      stopIterator = async () => {
        if (iterator.return !== undefined) await iterator.return()
      }
      const pump = async (): Promise<void> => {
        while (resources.readinessWatches.has(handle)) {
          const item = await iterator.next()
          if (item.done === true) break
          const value = item.value
          const delivery = await this.publish(
            String(resources.rendererLease.leaseId),
            this.event(resources.rendererLease, handle, readinessIpcItem(value))
          )
          if (delivery === 'terminalized' || value.kind === 'terminal') break
        }
      }
      pump().catch(async error => {
        console.error('[ElectronMainBleRouter] Write-readiness watch failed:', error)
        try {
          await this.publish(
            String(resources.rendererLease.leaseId),
            this.event(resources.rendererLease, handle, {
              kind: 'terminal',
              reason: 'source-failed',
              droppedItems: 0,
              droppedBytes: 0,
              replacedItems: 0,
              error: serializeNormalizedError(normalizedCleanupError(error))
            })
          )
        } catch (deliveryError) {
          console.error('[ElectronMainBleRouter] Write-readiness failure delivery failed:', deliveryError)
        }
      })
      return Object.freeze({ handle, connectionId: String(connection.connectionId) })
    } finally {
      resources.reservedHandles.delete(handle)
    }
  }

  private async unsubscribeWriteReadiness(resources: RendererResources, handle: string): Promise<SerializableRecord> {
    const watch = resources.readinessWatches.get(handle)
    if (watch === undefined) {
      if (resources.releasedHandles.has(handle)) return alreadyReleasedCleanup()
      throw contractError('ownership.denied', 'ipc', 'electron.write-readiness.owner')
    }
    if (watch.cleanup !== null) return watch.cleanup
    const tracked = (async () => {
      const failures: CleanupFailure[] = []
      try {
        await watch.stopIterator()
      } catch (error) {
        failures.push({ resourceKind: 'gatt.write-readiness', error: normalizedCleanupError(error) })
      }
      try {
        const native = await watch.closeWatch()
        if (native.state !== 'released') failures.push(...native.failures)
      } catch (error) {
        failures.push({ resourceKind: 'gatt.write-readiness', error: normalizedCleanupError(error) })
      }
      if (failures.length === 0) {
        resources.readinessWatches.delete(handle)
        resources.releasedHandles.add(handle)
        return cleanupRecord({ state: 'released', failures })
      }
      watch.cleanup = null
      return cleanupRecord({ state: 'release-failed', failures })
    })()
    watch.cleanup = tracked
    return tracked
  }

  private async releaseReadinessWatches(
    resources: RendererResources,
    connectionHandle?: string
  ): Promise<CleanupFailure[]> {
    const failures: CleanupFailure[] = []
    for (const [handle, watch] of resources.readinessWatches) {
      if (connectionHandle !== undefined && watch.connectionHandle !== connectionHandle) continue
      try {
        const cleanup = await this.unsubscribeWriteReadiness(resources, handle)
        if (cleanup.state !== 'released') {
          failures.push({
            resourceKind: 'gatt.write-readiness',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.write-readiness.release').normalized
          })
        }
      } catch (error) {
        failures.push({ resourceKind: 'gatt.write-readiness', error: normalizedCleanupError(error) })
      }
    }
    return failures
  }

  private async readConnectionParameters(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const connection = requiredResource(
      resources.connections,
      requiredString(payload, 'connectionHandle'),
      'connection'
    )
    const result = await connection.parameters(operationOptions(payload, controller))
    return Object.freeze({
      intervalUs: result.intervalUs,
      latency: result.latency,
      supervisionTimeoutUs: result.supervisionTimeoutUs,
      connectionId: String(result.connectionId),
      connectionGeneration: String(result.connectionGeneration),
      observedAtMonotonicMs: result.observedAtMonotonicMs
    })
  }

  private async subscribeConnectionParameters(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const connectionHandle = requiredString(payload, 'connectionHandle')
    const handle = requiredString(payload, 'parameterEventsHandle')
    const connection = requiredResource(resources.connections, connectionHandle, 'connection')
    if (
      resources.parameterWatches.has(handle) ||
      resources.releasedHandles.has(handle) ||
      resources.reservedHandles.has(handle)
    ) {
      throw contractError('protocol.violation', 'ipc', 'electron.connection-parameters.duplicate')
    }
    resources.reservedHandles.add(handle)
    try {
      const opened = await connection.parameterEvents(operationOptions(payload, controller))
      let stopIterator = async (): Promise<void> => undefined
      resources.parameterWatches.set(handle, {
        connectionHandle,
        closeWatch: () => opened.close(),
        stopIterator: () => stopIterator(),
        cleanup: null
      })
      acquisitions.parameterWatches.add(handle)
      const iterator = opened.events[Symbol.asyncIterator]()
      stopIterator = async () => {
        if (iterator.return !== undefined) await iterator.return()
      }
      const pump = async (): Promise<void> => {
        while (resources.parameterWatches.has(handle)) {
          const item = await iterator.next()
          if (item.done === true) break
          const value = item.value
          const delivery = await this.publish(
            String(resources.rendererLease.leaseId),
            this.event(resources.rendererLease, handle, parameterIpcItem(value))
          )
          if (delivery === 'terminalized' || value.kind === 'terminal') break
        }
      }
      pump().catch(async error => {
        console.error('[ElectronMainBleRouter] Connection-parameter watch failed:', error)
        try {
          await this.publish(
            String(resources.rendererLease.leaseId),
            this.event(resources.rendererLease, handle, {
              kind: 'terminal',
              reason: 'source-failed',
              droppedItems: 0,
              droppedBytes: 0,
              replacedItems: 0,
              error: serializeNormalizedError(normalizedCleanupError(error))
            })
          )
        } catch (deliveryError) {
          console.error('[ElectronMainBleRouter] Connection-parameter failure delivery failed:', deliveryError)
        }
      })
      return Object.freeze({ handle, connectionId: String(connection.connectionId) })
    } finally {
      resources.reservedHandles.delete(handle)
    }
  }

  private async unsubscribeConnectionParameters(
    resources: RendererResources,
    handle: string
  ): Promise<SerializableRecord> {
    const watch = resources.parameterWatches.get(handle)
    if (watch === undefined) {
      if (resources.releasedHandles.has(handle)) return alreadyReleasedCleanup()
      throw contractError('ownership.denied', 'ipc', 'electron.connection-parameters.owner')
    }
    if (watch.cleanup !== null) return watch.cleanup
    const tracked = (async () => {
      const failures: CleanupFailure[] = []
      try {
        await watch.stopIterator()
      } catch (error) {
        failures.push({ resourceKind: 'connection.parameters', error: normalizedCleanupError(error) })
      }
      try {
        const native = await watch.closeWatch()
        if (native.state !== 'released') failures.push(...native.failures)
      } catch (error) {
        failures.push({ resourceKind: 'connection.parameters', error: normalizedCleanupError(error) })
      }
      if (failures.length === 0) {
        resources.parameterWatches.delete(handle)
        resources.releasedHandles.add(handle)
        return cleanupRecord({ state: 'released', failures })
      }
      watch.cleanup = null
      return cleanupRecord({ state: 'release-failed', failures })
    })()
    watch.cleanup = tracked
    return tracked
  }

  private async releaseParameterWatches(
    resources: RendererResources,
    connectionHandle?: string
  ): Promise<CleanupFailure[]> {
    const failures: CleanupFailure[] = []
    for (const [handle, watch] of resources.parameterWatches) {
      if (connectionHandle !== undefined && watch.connectionHandle !== connectionHandle) continue
      try {
        const cleanup = await this.unsubscribeConnectionParameters(resources, handle)
        if (cleanup.state !== 'released') {
          failures.push({
            resourceKind: 'connection.parameters',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.connection-parameters.release')
              .normalized
          })
        }
      } catch (error) {
        failures.push({ resourceKind: 'connection.parameters', error: normalizedCleanupError(error) })
      }
    }
    return failures
  }

  private async unwatchSecurity(resources: RendererResources, handle: string): Promise<SerializableRecord> {
    const watch = resources.securityWatches.get(handle)
    if (watch === undefined) {
      if (resources.releasedHandles.has(handle)) return { state: 'released', failures: [] }
      throw contractError('ownership.denied', 'ipc', 'electron.security.watch-owner')
    }
    if (watch.cleanup !== null) return watch.cleanup
    const tracked = (async () => {
      const retire = (): void => {
        if (watch.nativeReleased && watch.localReturned && resources.securityWatches.get(handle) === watch) {
          resources.securityWatches.delete(handle)
          resources.releasedHandles.add(handle)
        }
      }
      if (!watch.localReturned && watch.localReturn === null) {
        watch.localReturn = Promise.resolve()
          .then(() => watch.iterator.return())
          .then(
            () => {
              watch.localReturned = true
              retire()
            },
            error => {
              watch.localReturn = null
              throw error
            }
          )
      }
      const drain = waitForChildCleanup(watch.localReturn ?? Promise.resolve())
      const failures: CleanupFailure[] = []
      if (!watch.nativeReleased && watch.nativeClose === null) {
        watch.nativeClose = Promise.resolve()
          .then(() => watch.stream.close())
          .then(
            native => {
              watch.nativeReleased = native.state === 'released'
              if (!watch.nativeReleased) watch.nativeClose = null
              retire()
              return native
            },
            error => {
              watch.nativeClose = null
              throw error
            }
          )
      }
      const nativeOutcome: { value: CleanupRecord | null } = { value: null }
      const nativeDrain = waitForChildCleanup(
        (watch.nativeClose ?? Promise.resolve({ state: 'released' as const, failures: [] })).then(value => {
          nativeOutcome.value = value
        })
      )
      const [local, native] = await Promise.all([drain, nativeDrain])
      if (nativeOutcome.value !== null) failures.push(...nativeOutcome.value.failures)
      if (native.error !== undefined)
        failures.push({ resourceKind: 'security-watch', error: normalizedCleanupError(native.error) })
      if (native.pending)
        failures.push({
          resourceKind: 'security-watch',
          error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.security.watch-native-cleanup-pending')
            .normalized
        })
      if (local.error !== undefined)
        failures.push({ resourceKind: 'security-watch-iterator', error: normalizedCleanupError(local.error) })
      if (local.pending)
        failures.push({
          resourceKind: 'security-watch-iterator',
          error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.security.watch-local-cleanup-pending')
            .normalized
        })
      retire()
      if (!watch.nativeReleased || !watch.localReturned || failures.length > 0) {
        watch.cleanup = null
        return cleanupRecord({ state: 'release-failed', failures })
      }
      return cleanupRecord({ state: 'released', failures: [] })
    })().catch(error => {
      if (watch.cleanup === tracked) watch.cleanup = null
      throw error
    })
    watch.cleanup = tracked
    return tracked
  }

  private async adapterState(controller: AbortController): Promise<SerializableRecord> {
    const pending = this.manager.adapterState()
    const aborted = new Promise<never>((_, reject) => {
      const fail = (): void => {
        reject(contractError('operation.aborted', 'adapter', 'electron-main-router.adapter-state'))
      }
      if (controller.signal.aborted) {
        fail()
        return
      }
      controller.signal.addEventListener('abort', fail, { once: true })
    })
    try {
      const state = await Promise.race([pending, aborted])
      return Object.freeze({
        state: Object.freeze({
          availability: state.availability,
          authorization: state.authorization,
          power: state.power,
          heard: null,
          backendGeneration: String(state.backendGeneration),
          updatedAt: state.updatedAt,
          safeReason: state.safeReason
        })
      })
    } finally {
      pending.then(
        () => undefined,
        () => undefined
      )
    }
  }

  private async peerDirectory(
    command: string,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const peers = this.manager.attachedBackend.backend?.peers
    if (peers === undefined) throw contractError('capability.unsupported', 'connection', 'electron-main-router.peers')
    const controls = operationOptions(payload, controller)
    if (command === 'peers.resolve') {
      const reference = snapshotPeerReference(payload.reference, 'electron-main-router.peers.resolve')
      const peer = await peers.resolve(reference, controls)
      return { peer: peer === null ? null : encodePeerRecord(peer) }
    }
    const options = { ...decodePeerQuery(payload.query), ...controls }
    const records =
      command === 'peers.known'
        ? await peers.known(options)
        : command === 'peers.connected'
          ? await peers.connected(options)
          : command === 'peers.bonded'
            ? await peers.bonded(options)
            : command === 'peers.authorized'
              ? await peers.authorized(options)
              : command === 'peers.restored'
                ? await peers.restored(options)
                : null
    if (records === null) throw contractError('argument.invalid', 'ipc', 'electron-main-router.peers.command')
    return { peers: records.map(encodePeerRecord) }
  }

  private async maximumWriteLength(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const mode = payload.mode
    if (mode !== 'with-response' && mode !== 'without-response') {
      throw contractError('argument.invalid', 'ipc', 'electron-main-router.maximum-write-length.mode')
    }
    const connection = requiredResource(
      resources.connections,
      requiredString(payload, 'connectionHandle'),
      'connection'
    )
    const result = await connection.maximumWriteLength(mode, operationOptions(payload, controller))
    return Object.freeze({
      bytes: result.maximumWriteLength,
      mode,
      observedAtMonotonicMs: result.observedAtMonotonicMs
    })
  }

  private async readRssi(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const connection = requiredResource(
      resources.connections,
      requiredString(payload, 'connectionHandle'),
      'connection'
    )
    const result = await connection.readRssi(operationOptions(payload, controller))
    return Object.freeze({ rssi: result.rssi })
  }

  private async effectiveMtu(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const connection = requiredResource(
      resources.connections,
      requiredString(payload, 'connectionHandle'),
      'connection'
    )
    // Null is an unobserved ATT MTU (CoreBluetooth). A measured host sends
    // the number. The renderer maps null to state `unavailable`.
    const result = await connection.effectiveMtu(operationOptions(payload, controller))
    return Object.freeze({ mtu: result.attMtu })
  }

  private async discover(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const connectionHandle = requiredString(payload, 'connectionHandle')
    const connection = requiredResource(resources.connections, connectionHandle, 'connection')
    const reason = optionalRediscoveryReason(payload)
    const retirement = await this.retireDatabasesForRediscovery(resources, connectionHandle)
    if (retirement.state === 'release-failed') {
      throw new BackendContractError(
        retirement.failures[0]?.error ??
          contractError('platform.failure', 'cleanup', 'electron-main-router.rediscovery-retirement').normalized
      )
    }
    const database =
      reason === null
        ? await connection.discover(operationOptions(payload, controller))
        : await connection.rediscoverGatt(operationOptions(payload, controller), reason)
    const characteristics = new Map<string, MainCharacteristicPath>()
    const descriptors = new Map<string, MainDescriptorPath>()
    const handle = this.allocateHandle('database')
    resources.databases.set(handle, {
      connectionHandle,
      database,
      characteristics,
      descriptors
    })
    acquisitions.databases.add(handle)
    const snapshot = await database.snapshot()
    const characteristicHandles = new Map<string, string>()
    const serializedCharacteristics: SerializableValue[] = []
    const serializedDescriptors: SerializableValue[] = []
    const serializedServices: SerializableValue[] = (snapshot.services ?? []).map(service =>
      Object.freeze({
        uuid: String(service.path.serviceUuid),
        occurrence: String(service.path.serviceOccurrence),
        primary: service.primary,
        includedServices:
          service.includedServices === null
            ? null
            : service.includedServices.map(included =>
                Object.freeze({ uuid: String(included.uuid), occurrence: String(included.occurrence) })
              ),
        ...(service.restriction === undefined ? {} : { restriction: serializeServiceRestriction(service.restriction) })
      })
    )
    for (const characteristic of snapshot.characteristics ?? []) {
      const characteristicHandle = this.allocateHandle('characteristic')
      characteristics.set(characteristicHandle, characteristic.path)
      characteristicHandles.set(characteristicKey(characteristic.path), characteristicHandle)
      serializedCharacteristics.push(
        Object.freeze({
          handle: characteristicHandle,
          serviceUuid: String(characteristic.path.serviceUuid),
          serviceOccurrence: String(characteristic.path.serviceOccurrence),
          characteristicUuid: String(characteristic.path.characteristicUuid),
          characteristicOccurrence: String(characteristic.path.characteristicOccurrence),
          properties: characteristicProperties(characteristic),
          ...(characteristic.properties === undefined
            ? {}
            : { propertiesMetadata: serializeCharacteristicProperties(characteristic.properties) }),
          ...(characteristic.access === undefined ? {} : { access: serializeAccessRequirements(characteristic.access) })
        })
      )
    }
    for (const descriptor of snapshot.descriptors ?? []) {
      const characteristicHandle = characteristicHandles.get(characteristicKey(descriptor.path))
      if (characteristicHandle === undefined) {
        throw contractError('protocol.violation', 'gatt', 'electron-main-router.descriptor-parent')
      }
      const descriptorHandle = this.allocateHandle('descriptor')
      descriptors.set(descriptorHandle, descriptor.path)
      serializedDescriptors.push(
        Object.freeze({
          handle: descriptorHandle,
          characteristicHandle,
          uuid: String(descriptor.path.descriptorUuid),
          occurrence: String(descriptor.path.descriptorOccurrence),
          ...(descriptor.properties === undefined
            ? {}
            : { properties: serializeDescriptorProperties(descriptor.properties) })
        })
      )
    }
    const response = Object.freeze({
      schemaVersion: 2,
      handle,
      databaseId: String(database.path?.databaseId ?? ''),
      databaseGeneration: String(database.path?.databaseGeneration ?? ''),
      services: Object.freeze(serializedServices),
      characteristics: Object.freeze(serializedCharacteristics),
      descriptors: Object.freeze(serializedDescriptors)
    })
    return reason === null ? response : Object.freeze({ ...response, rediscoveryReason: reason })
  }

  private async read(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const database = this.database(resources, payload)
    const path = this.characteristic(database, payload)
    const { value, provenance } = await database.database.readReceipt(path, operationOptions(payload, controller))
    return Object.freeze({ value: ownBytes(value, byteLimit(value.byteLength)), provenance })
  }

  private async write(
    resources: RendererResources,
    payload: SerializableRecord,
    binaryPayload: OwnedBytes | null,
    controller: AbortController,
    waitReady = false
  ): Promise<SerializableRecord> {
    if (binaryPayload === null) {
      throw contractError('bytes.invalid', 'ipc', 'electron-main-router.write-missing-bytes')
    }
    const database = this.database(resources, payload)
    const path = this.characteristic(database, payload)
    const mode = requiredWriteMode(payload)
    if (waitReady && mode !== 'without-response') {
      throw contractError('argument.invalid', 'gatt', 'electron-main-router.write-when-ready.mode')
    }
    const write = waitReady ? database.database.writeWhenReady : database.database.write
    if (write === undefined) {
      throw contractError('capability.unsupported', 'gatt', 'electron-main-router.write-when-ready')
    }
    const receipt = await write.call(database.database, path, new Uint8Array(binaryPayload), {
      ...operationOptions(payload, controller),
      mode
    })
    return serializeWriteReceipt(receipt, mode, binaryPayload.byteLength)
  }

  private async readDescriptor(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const database = this.database(resources, payload)
    const path = this.descriptor(database, payload)
    const value = await database.database.readDescriptor(path, operationOptions(payload, controller))
    return Object.freeze({ value: ownBytes(value, byteLimit(value.byteLength)) })
  }

  private async writeDescriptor(
    resources: RendererResources,
    payload: SerializableRecord,
    binaryPayload: OwnedBytes | null,
    controller: AbortController
  ): Promise<SerializableRecord> {
    if (binaryPayload === null) {
      throw contractError('bytes.invalid', 'ipc', 'electron-main-router.descriptor-write-missing-bytes')
    }
    const database = this.database(resources, payload)
    const path = this.descriptor(database, payload)
    const receipt = await database.database.writeDescriptor(path, new Uint8Array(binaryPayload), {
      ...operationOptions(payload, controller),
      mode: 'with-response'
    })
    return serializeWriteReceipt(receipt, 'with-response', binaryPayload.byteLength)
  }

  private async subscribe<Renderer extends string, Operation extends string>(
    resources: RendererResources,
    envelope: IpcEnvelope<string, Renderer, Operation>,
    controller: AbortController,
    acquisitions: OperationAcquisitions,
    acquired = false
  ): Promise<SerializableRecord> {
    const database = this.database(resources, envelope.payload)
    const path = this.characteristic(database, envelope.payload)
    const deliveryMode = requiredDeliveryMode(envelope.payload.deliveryMode)
    const options = {
      ...operationOptions(envelope.payload, controller),
      delivery: deliveryFromPayload(envelope.payload),
      ...(deliveryMode === undefined ? {} : { deliveryMode })
    } satisfies SubscriptionOptions
    const notifications = acquired ? await database.database.acquireNotifications(path, options) : null
    const ordinary = notifications === null ? await database.database.subscribe(path, options) : null
    const subscription =
      notifications === null ? ordinary : { values: notifications.values, remove: () => notifications.close() }
    if (subscription === null)
      throw contractError('lifecycle.invariant-violation', 'gatt', 'electron.gatt.subscription-admission')
    const handle = this.allocateHandle('subscription')
    this.streams.registerSubscription(
      resources,
      envelope.rendererLease,
      handle,
      requiredString(envelope.payload, 'databaseHandle'),
      subscription
    )
    acquisitions.subscriptions.add(handle)
    return Object.freeze({
      handle,
      observedDelivery: ordinary?.observedDelivery ?? 'unknown',
      ...(notifications === null ? {} : { mtuBytes: notifications.mtuBytes })
    })
  }

  private async acquireWriter(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const database = this.database(resources, payload)
    const writer = await database.database.acquireWrite(
      this.characteristic(database, payload),
      operationOptions(payload, controller)
    )
    const handle = this.allocateHandle('acquired-writer')
    resources.acquiredWriters.set(handle, {
      writer,
      databaseHandle: requiredString(payload, 'databaseHandle'),
      connectionHandle: database.connectionHandle,
      cleanup: null
    })
    acquisitions.acquiredWriters.add(handle)
    return Object.freeze({ handle, mtuBytes: writer.mtuBytes })
  }

  private async writeAcquired(
    resources: RendererResources,
    payload: SerializableRecord,
    bytes: OwnedBytes | null,
    controller: AbortController
  ): Promise<SerializableRecord> {
    if (bytes === null) throw contractError('bytes.invalid', 'ipc', 'electron.acquired-write.bytes')
    const entry = requiredResource(
      resources.acquiredWriters,
      requiredString(payload, 'acquiredHandle'),
      'acquired-writer'
    )
    const database = this.database(resources, payload)
    if (resources.databases.get(entry.databaseHandle) !== database)
      throw contractError('ownership.denied', 'gatt', 'electron.acquired-write.database')
    const receipt = await entry.writer.write(new Uint8Array(bytes), operationOptions(payload, controller))
    return serializeWriteReceipt(receipt, 'without-response', bytes.byteLength)
  }

  private closeAcquiredWriter(resources: RendererResources, handle: string): Promise<CleanupRecord> {
    const entry = resources.acquiredWriters.get(handle)
    if (entry === undefined) {
      if (releasedHandleTombstone(resources, handle)) return Promise.resolve({ state: 'released', failures: [] })
      throw contractError('ownership.denied', 'ipc', 'electron.acquired-write.owner')
    }
    if (entry.cleanup !== null) return entry.cleanup
    const result = entry.writer.close().then(
      cleanup => {
        if (cleanup.state === 'released') {
          resources.acquiredWriters.delete(handle)
          resources.releasedHandles.add(handle)
        } else entry.cleanup = null
        return cleanup
      },
      error => {
        entry.cleanup = null
        throw error
      }
    )
    entry.cleanup = result
    return result
  }

  private async releaseAcquiredWriters(
    resources: RendererResources,
    include: (entry: ManagedAcquiredWriter, handle: string) => boolean = () => true
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const [handle, entry] of resources.acquiredWriters) {
      if (!include(entry, handle)) continue
      try {
        failures.push(...(await this.closeAcquiredWriter(resources, handle)).failures)
      } catch (error) {
        failures.push({ resourceKind: 'acquired-gatt-writer', error: normalizedCleanupError(error) })
      }
    }
    return { state: failures.length === 0 ? 'released' : 'release-failed', failures }
  }

  private async stopScan(resources: RendererResources, payload: SerializableRecord): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'scanHandle')
    const resource = requiredResource(resources.scans, handle, 'scan')
    const cleanup = await this.streams.stopScan(resources, handle, resource, true)
    return cleanupRecord(cleanup)
  }

  private async disconnect(resources: RendererResources, payload: SerializableRecord): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'connectionHandle')
    const connection = resources.connections.get(handle)
    if (connection === undefined) {
      if (releasedHandleTombstone(resources, handle)) return alreadyReleasedCleanup()
      throw contractError('ownership.denied', 'ipc', 'electron-main-router.connection-ownership')
    }
    const writerCleanup = await this.releaseAcquiredWriters(resources, entry => entry.connectionHandle === handle)
    if (writerCleanup.state === 'release-failed') return cleanupRecord(writerCleanup)
    const readinessCleanup = await this.releaseReadinessWatches(resources, handle)
    if (readinessCleanup.length > 0) {
      return cleanupRecord({ state: 'release-failed', failures: readinessCleanup })
    }
    const parameterCleanup = await this.releaseParameterWatches(resources, handle)
    if (parameterCleanup.length > 0) {
      return cleanupRecord({ state: 'release-failed', failures: parameterCleanup })
    }
    const lifecycleCleanup = await this.releaseConnectionEventSubscriptionsForConnection(resources, handle)
    if (lifecycleCleanup.state === 'release-failed') {
      return cleanupRecord(lifecycleCleanup)
    }
    const orderedCleanup = await this.releaseOrderedGroups(
      resources,
      group => resources.databases.get(group.databaseHandle)?.connectionHandle === handle
    )
    if (orderedCleanup.state === 'release-failed') return cleanupRecord(orderedCleanup)
    const subscriptionCleanup = await this.releaseSubscriptionsForConnection(resources, handle)
    if (subscriptionCleanup.state === 'release-failed') {
      return cleanupRecord(subscriptionCleanup)
    }
    const cleanup = await this.disconnectConnection(resources, handle, connection)
    if (cleanup.state === 'released') {
      this.deleteDatabasesForConnection(resources, handle)
    }
    return cleanupRecord(cleanup)
  }

  private subscribeConnectionEvents(
    resources: RendererResources,
    payload: SerializableRecord,
    acquisitions: OperationAcquisitions
  ): ElectronConnectionEventsSubscribeResponseV2 {
    const connectionHandle = requiredString(payload, 'connectionHandle')
    const connection = requiredResource(resources.connections, connectionHandle, 'connection')
    const handle = requiredString(payload, 'connectionEventsHandle')
    if (!isElectronConnectionEventsStreamHandle(handle)) {
      throw contractError('argument.invalid', 'ipc', 'electron-main-router.connection-events-handle')
    }
    const response = this.connectionEvents.register(resources, handle, connectionHandle, connection, this.attachment)
    acquisitions.connectionEventSubscriptions.add(handle)
    return response
  }

  private readyConnectionEvents(resources: RendererResources, payload: SerializableRecord): SerializableRecord {
    const handle = requiredString(payload, 'connectionEventsHandle')
    this.connectionEvents.ready(resources, resources.rendererLease, handle)
    return Object.freeze({ state: 'ready' })
  }

  private async unsubscribeConnectionEvents(
    resources: RendererResources,
    payload: SerializableRecord
  ): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'connectionEventsHandle')
    const resource = resources.connectionEventSubscriptions.get(handle)
    if (resource === undefined) {
      if (releasedHandleTombstone(resources, handle)) return alreadyReleasedCleanup()
      throw contractError('ownership.denied', 'ipc', 'electron-main-router.connection-events-ownership')
    }
    const cleanup = await this.connectionEvents.remove(resources, handle, resource, true)
    return cleanupRecord(cleanup)
  }

  private recordingLease(resources: RendererResources): string {
    return JSON.stringify([this.registryScope, String(resources.rendererLease.leaseId)])
  }

  private async orderedMarker(
    resources: RendererResources,
    command: string,
    payload: SerializableRecord,
    binary: OwnedBytes | null,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const recording = command.startsWith('gatt.ordered-recording.')
    const handle = requiredString(payload, recording ? 'startHandle' : 'orderedHandle')
    const group = recording ? null : requiredResource(resources.orderedGroups, handle, 'ordered-group')
    const key = recording ? JSON.stringify([resources.ownerKey, 'recording', handle]) : group?.markerKey
    if (recording) this.orderedRecordings.assertOwned(resources.ownerKey, handle)
    if (key === undefined) throw contractError('ownership.denied', 'ipc', 'electron.ordered-marker.owner')
    const action = command.slice(command.lastIndexOf('.') + 1)
    if (action === 'list' && recording) {
      if (binary !== null) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.binary')
      return this.orderedMarkers.list(key)
    }
    if (action === 'reserve') {
      if (binary !== null) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.binary')
      if (group !== null && group.setup.state !== 'ready')
        throw contractError('lifecycle.invalid-state', 'gatt', 'electron.ordered-marker.group')
      if (recording) {
        this.orderedRecordings.assertMarkerOwner(resources.ownerKey, handle)
        const lease = this.recordingLease(resources)
        if (
          !this.orderedRecordings.isOriginLease(resources.ownerKey, handle, lease) &&
          !this.orderedRecovery.hasMapping(resources.ownerKey, handle, lease)
        )
          throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.mapping')
      }
      return this.orderedMarkers.reserve(
        key,
        requiredString(payload, 'reservationToken'),
        this.recordingLease(resources)
      )
    }
    const markerId = requiredString(payload, 'markerHandle')
    if (!isCanonicalIngressOrdinal(markerId))
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.id')
    if (action === 'status' || action === 'cancel') {
      if (binary !== null) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.binary')
      return this.orderedMarkers.status(key, markerId, action === 'cancel')
    }
    if (action === 'forget') {
      if (binary !== null) throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.binary')
      const result = await this.orderedMarkers.forget(
        key,
        markerId,
        ordinal => recording || (group?.acknowledgedMarkerOrdinals.has(ordinal) ?? false)
      )
      group?.acknowledgedMarkerOrdinals.clear()
      return result
    }
    if (action !== 'start' || binary === null || typeof payload.withResponse !== 'boolean')
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered-marker.start')
    const database = this.database(resources, payload)
    if (group !== null && group.databaseHandle !== requiredString(payload, 'databaseHandle'))
      throw contractError('ownership.denied', 'ipc', 'electron.ordered-marker.database')
    if (group !== null && group.setup.state !== 'ready')
      throw contractError('lifecycle.invalid-state', 'gatt', 'electron.ordered-marker.group')
    const target = this.characteristic(database, payload)
    const bytes = new Uint8Array(binary)
    const fingerprint = orderedWireRecord({
      databaseHandle: requiredString(payload, 'databaseHandle'),
      characteristicHandle: requiredString(payload, 'characteristicHandle'),
      withResponse: payload.withResponse,
      bytes
    })
    return this.orderedMarkers.start(
      key,
      markerId,
      fingerprint,
      () => {
        if (recording) {
          const connection = requiredResource(resources.connections, database.connectionHandle, 'connection')
          const lease = this.recordingLease(resources)
          const databaseHandle = requiredString(payload, 'databaseHandle')
          if (
            !this.orderedRecordings.isOriginLease(resources.ownerKey, handle, lease) &&
            !this.orderedRecovery.hasMapping(resources.ownerKey, handle, lease, databaseHandle)
          )
            throw contractError('ownership.denied', 'ipc', 'electron.recording-recovery.mapping')
          const recordingId = this.orderedRecordings.markerRecordingId(
            resources.ownerKey,
            handle,
            String(connection.peerId)
          )
          const reserve = database.database.reserveOrderedRecordingMarker
          if (!reserve) throw contractError('capability.unsupported', 'gatt', 'electron.ordered-recording.marker')
          return reserve.call(database.database, recordingId)
        }
        const reserve = group?.setup.stream.reserveMarkerWrite
        if (!reserve || group === null) throw contractError('capability.unsupported', 'gatt', 'electron.ordered-marker')
        return reserve.call(group.setup.stream)
      },
      target,
      bytes,
      payload.withResponse,
      operationOptions(payload, controller)
    )
  }

  private async startOrderedRecording(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController
  ): Promise<SerializableRecord> {
    const database = this.database(resources, payload)
    const handle = requiredString(payload, 'startHandle')
    if (!isCanonicalIngressOrdinal(handle))
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.handle')
    const recording = normalizeContinuationRecording(payload.recording)
    if (!Array.isArray(payload.members) || payload.members.length === 0)
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.members')
    const members = payload.members.map(entry => {
      if (entry === null || typeof entry !== 'object' || Array.isArray(entry) || entry instanceof Uint8Array)
        throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.member')
      const deliveryMode = requiredDeliveryMode(entry.deliveryMode)
      if (deliveryMode === undefined)
        throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.delivery')
      return { memberId: requiredString(entry, 'memberId'), path: this.characteristic(database, entry), deliveryMode }
    })
    if (
      new Set(members.map(member => member.memberId)).size !== members.length ||
      new Set(
        payload.members.map(entry => {
          if (entry === null || typeof entry !== 'object' || Array.isArray(entry) || entry instanceof Uint8Array)
            throw contractError('protocol.malformed', 'ipc', 'electron.ordered-recording.member')
          return requiredString(entry, 'characteristicHandle')
        })
      ).size !== members.length
    )
      throw contractError('argument.invalid', 'gatt', 'electron.ordered-recording.duplicate')
    const delivery = deliveryFromPayload({ ...payload, streamOverflowPolicy: 'error' })
    const fingerprint = snapshotSerializableRecord({
      databaseHandle: requiredString(payload, 'databaseHandle'),
      members: payload.members,
      recording: orderedWireRecord(recording),
      streamItemCapacity: Number(delivery.itemCapacity),
      streamByteCapacity: Number(delivery.byteCapacity),
      streamReservedControlCapacity: Number(delivery.reservedControlCapacity)
    }).value
    const connection = requiredResource(resources.connections, database.connectionHandle, 'connection')
    return this.orderedRecordings.start(
      resources.ownerKey,
      handle,
      recording.id,
      String(connection.peerId),
      fingerprint,
      () => database.database.reserveOrderedRecordingStart(),
      owner => owner.start(members, { ...operationOptions(payload, controller), delivery, recording })
    )
  }

  /** Trusted process owner reports a confirmed finished recorder and host cleanup. */
  confirmOrderedRecordingFinished(
    recordingId: string,
    result: import('../core/continuation-recording').OrderedRecordingHostStopResult
  ): void {
    confirmProcessRecordingFinished(
      { recordings: this.orderedRecordings, markers: this.orderedMarkers, recovery: this.orderedRecovery },
      recordingId,
      result
    )
  }

  private async openOrdered(
    resources: RendererResources,
    payload: SerializableRecord,
    controller: AbortController,
    acquisitions: OperationAcquisitions
  ): Promise<SerializableRecord> {
    const database = this.database(resources, payload)
    const handle = requiredString(payload, 'orderedHandle')
    const openRequest = snapshotSerializableRecord({
      databaseHandle: requiredString(payload, 'databaseHandle'),
      members: payload.members ?? null,
      streamItemCapacity: payload.streamItemCapacity ?? null,
      streamByteCapacity: payload.streamByteCapacity ?? null,
      streamReservedControlCapacity: payload.streamReservedControlCapacity ?? null,
      streamOverflowPolicy: payload.streamOverflowPolicy ?? null
    }).value
    const existing = resources.orderedGroups.get(handle)
    if (existing !== undefined) {
      if (!serializableRecordsEqual(existing.openRequest, openRequest))
        throw contractError('protocol.violation', 'ipc', 'electron.ordered.open-retry')
      return existing.openResponse
    }
    if (
      resources.reservedHandles.has(handle) ||
      resources.releasedOrderedGroups.has(handle) ||
      resources.releasedHandles.has(handle)
    )
      throw contractError('ownership.denied', 'ipc', 'electron.ordered.handle')
    if (
      resources.orderedGroups.size + resources.releasedOrderedGroups.size + resources.reservedHandles.size >=
      this.maximumOutstandingOperations
    )
      throw contractError('stream.quota', 'ipc', 'electron.ordered.group-capacity')
    if (!Array.isArray(payload.members) || payload.members.length === 0)
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered.members')
    const members = payload.members.map(entry => {
      if (entry === null || typeof entry !== 'object' || Array.isArray(entry) || entry instanceof Uint8Array)
        throw contractError('protocol.malformed', 'ipc', 'electron.ordered.member')
      const deliveryMode = requiredDeliveryMode(entry.deliveryMode)
      if (deliveryMode === undefined) throw contractError('protocol.malformed', 'ipc', 'electron.ordered.delivery')
      return { memberId: requiredString(entry, 'memberId'), path: this.characteristic(database, entry), deliveryMode }
    })
    resources.reservedHandles.add(handle)
    try {
      if (
        new Set(members.map(member => member.memberId)).size !== members.length ||
        new Set(
          payload.members.map(entry => {
            if (entry === null || typeof entry !== 'object' || Array.isArray(entry) || entry instanceof Uint8Array)
              throw contractError('protocol.malformed', 'ipc', 'electron.ordered.member')
            return requiredString(entry, 'characteristicHandle')
          })
        ).size !== members.length
      )
        throw contractError('argument.invalid', 'gatt', 'electron.ordered.duplicate-member')
      const setup = await database.database.subscribeOrdered(members, {
        ...operationOptions(payload, controller),
        delivery: deliveryFromPayload(payload)
      })
      let iterator = setup.stream[Symbol.asyncIterator]()
      const pendingSetupObservations: OrderedNotificationItem[] = []
      let openResponse =
        setup.state === 'ready'
          ? orderedWireRecord({
              handle,
              state: setup.state,
              sourceGeneration: setup.sourceGeneration ?? null,
              members: setup.members.map(member => ({
                memberId: member.memberId,
                requestedDelivery: member.requestedDelivery,
                effectiveDelivery: member.effectiveDelivery
              })),
              limits: setup.stream.limits
            })
          : orderedWireRecord({
              handle,
              state: setup.state,
              sourceGeneration: setup.sourceGeneration ?? null,
              members: setup.members,
              error: setup.error,
              observations: setup.observations,
              uncertainTail: setup.uncertainTail,
              cleanup: setup.cleanup
            })
      if (setup.state === 'failed' && snapshotSerializableRecord(openResponse).byteLength > this.maximumMessageBytes) {
        const remaining = iterator
        pendingSetupObservations.push(...setup.observations)
        iterator = (async function* (): AsyncGenerator<OrderedNotificationItem, undefined, undefined> {
          while (pendingSetupObservations.length > 0) {
            const item = pendingSetupObservations.shift()
            if (item !== undefined) yield item
          }
          for (;;) {
            const next = await remaining.next()
            if (next.done) return undefined
            yield next.value
          }
        })()
        openResponse = orderedWireRecord({
          handle,
          state: setup.state,
          sourceGeneration: setup.sourceGeneration ?? null,
          members: setup.members,
          error: setup.error,
          observations: [],
          uncertainTail: setup.uncertainTail,
          cleanup: setup.cleanup
        })
      }
      const delivery = new OrderedDelivery<OrderedNotificationItem>(iterator, error => ({
        kind: 'terminal',
        reason: 'source-failed',
        uncertainTail: true,
        error: normalizedCleanupError(error)
      }))
      resources.orderedGroups.set(handle, {
        databaseHandle: requiredString(payload, 'databaseHandle'),
        setup,
        delivery,
        openResponse,
        openRequest,
        pendingSetupObservations,
        markerKey: JSON.stringify([resources.ownerKey, this.recordingLease(resources), handle]),
        acknowledgedMarkerOrdinals: new Set(),
        cleanupReleased: false,
        cleanupResponse: null
      })
      acquisitions.orderedGroups.add(handle)
      return openResponse
    } finally {
      resources.reservedHandles.delete(handle)
    }
  }

  /** Trusted desktop hosts inspect only payload-free summaries for their authenticated window. */
  inspectOrderedOrphans(ownerKey: string): SerializableRecord {
    const available = this.orphanReceipts.get(ownerKey)
    const receipts: SerializableRecord[] = []
    for (const item of available?.values() ?? []) {
      const receipt = item.record
      if (typeof receipt.uncertainTail !== 'boolean')
        throw contractError('protocol.malformed', 'ipc', 'electron.ordered.orphan-summary')
      const summary = {
        format: requiredString(receipt, 'format'),
        receiptId: requiredString(receipt, 'receiptId'),
        values: requiredString(receipt, 'values'),
        bytes: requiredString(receipt, 'bytes'),
        controls: requiredString(receipt, 'controls'),
        uncertainTail: receipt.uncertainTail
      }
      if (
        snapshotSerializableRecord({ receipts: [...receipts, summary], truncated: true }).byteLength >
        this.maximumMessageBytes
      ) {
        if (receipts.length === 0) throw contractError('bytes.too-large', 'ipc', 'electron.ordered.orphan-summary-size')
        break
      }
      receipts.push(summary)
    }
    return { receipts, truncated: receipts.length < (available?.size ?? 0) }
  }

  private ownedOrderedOrphan(ownerKey: string, receiptId: string): ManagedOrphanReceipt | null {
    const item = this.orphanReceipts.get(ownerKey)?.get(receiptId)
    if (item !== undefined) return item
    for (const [otherOwner, receipts] of this.orphanReceipts) {
      if (otherOwner !== ownerKey && receipts.has(receiptId))
        throw contractError('ownership.denied', 'ipc', 'electron.ordered.orphan-ownership')
    }
    return null
  }

  pageOrderedOrphan(ownerKey: string, payload: SerializableRecord): SerializableRecord {
    const item = this.ownedOrderedOrphan(ownerKey, requiredString(payload, 'receiptId'))
    if (item === null) throw contractError('ownership.denied', 'ipc', 'electron.ordered.orphan-unknown')
    const offset = payload.offset
    if (typeof offset !== 'number' || !Number.isSafeInteger(offset) || offset < 0)
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered.orphan-offset')
    if (offset === 0 && item.cursor !== 0) {
      item.cursor = 0
      item.nextOffset = null
      item.page = null
    }
    if (offset === item.cursor && item.page !== null) return item.page
    if (item.nextOffset !== null && offset === item.nextOffset) {
      item.cursor = offset
      item.page = null
      item.nextOffset = null
    } else if (offset !== item.cursor)
      throw contractError('protocol.violation', 'ipc', 'electron.ordered.orphan-offset')
    if (offset === item.scalars.length) {
      item.complete = true
      item.page = { chunk: '', nextOffset: offset, done: true }
      return item.page
    }
    let length = Math.min(
      4096,
      item.scalars.length - offset,
      Math.max(1, Math.floor((this.maximumMessageBytes - 256) / 12))
    )
    let page: SerializableRecord = {
      chunk: item.scalars.slice(offset, offset + length).join(''),
      nextOffset: offset + length,
      done: false
    }
    while (snapshotSerializableRecord(page).byteLength > this.maximumMessageBytes && length > 1) {
      length = Math.max(1, Math.floor(length / 2))
      page = { chunk: item.scalars.slice(offset, offset + length).join(''), nextOffset: offset + length, done: false }
    }
    if (snapshotSerializableRecord(page).byteLength > this.maximumMessageBytes)
      throw contractError('bytes.too-large', 'ipc', 'electron.ordered.orphan-page-size')
    item.nextOffset = offset + length
    item.page = page
    return page
  }

  acknowledgeOrderedOrphan(ownerKey: string, receiptId: string): SerializableRecord {
    const item = this.ownedOrderedOrphan(ownerKey, receiptId)
    if (item !== null && !item.complete)
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered.orphan-unread')
    const receipts = this.orphanReceipts.get(ownerKey)
    receipts?.delete(receiptId)
    if (receipts?.size === 0) this.orphanReceipts.delete(ownerKey)
    return { state: 'forgotten' }
  }

  private statusOrdered(resources: RendererResources, payload: SerializableRecord): SerializableRecord {
    const handle = requiredString(payload, 'orderedHandle')
    const group = resources.orderedGroups.get(handle)
    if (group !== undefined) return { state: 'result', result: group.openResponse }
    if (resources.reservedHandles.has(handle)) return { state: 'pending' }
    return { state: 'unknown' }
  }

  private forgetOrdered(resources: RendererResources, payload: SerializableRecord): SerializableRecord {
    const handle = requiredString(payload, 'orderedHandle')
    const group = resources.orderedGroups.get(handle)
    if (group !== undefined) {
      if (
        !group.cleanupReleased ||
        group.delivery === null ||
        !group.delivery.drained ||
        this.orderedMarkers.hasActive(group.markerKey)
      )
        throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered.forget-before-drain')
      resources.orderedGroups.delete(handle)
      this.orderedMarkers.releaseOwner(group.markerKey)
      if (group.cleanupResponse !== null) resources.releasedOrderedGroups.set(handle, group.cleanupResponse)
    }
    if (resources.reservedHandles.has(handle))
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered.forget-pending')
    if (!resources.releasedOrderedGroups.has(handle) && !resources.releasedHandles.has(handle))
      throw contractError('ownership.denied', 'ipc', 'electron.ordered.forget-owner')
    resources.forgottenOrderedGroups.add(handle)
    return { state: 'forgotten' }
  }

  private retireOrdered(resources: RendererResources, payload: SerializableRecord): SerializableRecord {
    const handle = requiredString(payload, 'orderedHandle')
    if (
      resources.orderedGroups.has(handle) ||
      resources.reservedHandles.has(handle) ||
      (resources.releasedOrderedGroups.has(handle) && !resources.forgottenOrderedGroups.has(handle))
    )
      throw contractError('lifecycle.invalid-state', 'ipc', 'electron.ordered.retire-before-forget')
    resources.releasedOrderedGroups.delete(handle)
    resources.forgottenOrderedGroups.delete(handle)
    return { state: 'retired' }
  }

  private async nextOrdered(resources: RendererResources, payload: SerializableRecord): Promise<SerializableRecord> {
    const group = requiredResource(resources.orderedGroups, requiredString(payload, 'orderedHandle'), 'ordered-group')
    if (group.delivery === null) return { state: 'closed' }
    const ack = payload.ackDeliveryId
    if (ack !== undefined && typeof ack !== 'string')
      throw contractError('protocol.malformed', 'ipc', 'electron.ordered.ack')
    const current = group.delivery.current
    if (ack !== undefined && current?.deliveryId === ack && current.item.kind === 'marker')
      group.acknowledgedMarkerOrdinals.add(current.item.ordinal)
    const result = group.delivery.poll(ack)
    if (
      result.state === 'item' &&
      snapshotSerializableRecord(orderedWireRecord(result)).byteLength > this.maximumMessageBytes
    ) {
      // Do not discard the stream to make room: later records may carry
      // independent failure evidence. Retain this delivery for retry/recovery.
      throw contractError('bytes.too-large', 'ipc', 'electron.ordered.record-size')
    }
    if (result.state === 'closed' && group.cleanupReleased && !this.orderedMarkers.hasActive(group.markerKey)) {
      const handle = requiredString(payload, 'orderedHandle')
      resources.orderedGroups.delete(handle)
      if (group.cleanupResponse !== null) resources.releasedOrderedGroups.set(handle, group.cleanupResponse)
      resources.releasedHandles.add(handle)
    }
    return orderedWireRecord(result)
  }

  private async closeOrdered(resources: RendererResources, payload: SerializableRecord): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'orderedHandle')
    const group = resources.orderedGroups.get(handle)
    if (group === undefined) {
      const released = resources.releasedOrderedGroups.get(handle)
      if (released !== undefined) return released
      throw contractError('ownership.denied', 'ipc', 'electron.ordered.ownership')
    }
    const cleanup = await (group.setup.state === 'ready' ? group.setup.stream.close() : group.setup.close())
    group.cleanupReleased =
      cleanup.group.state === 'released' && cleanup.members.every(member => member.cleanup.state === 'released')
    group.cleanupResponse = orderedWireRecord({ cleanup })
    if (
      group.cleanupReleased &&
      (group.delivery === null || group.delivery.drained) &&
      !this.orderedMarkers.hasActive(group.markerKey)
    ) {
      resources.orderedGroups.delete(handle)
      resources.releasedOrderedGroups.set(handle, group.cleanupResponse)
      resources.releasedHandles.add(handle)
    }
    return group.cleanupResponse
  }

  private async unsubscribe(resources: RendererResources, payload: SerializableRecord): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'subscriptionHandle')
    const resource = resources.subscriptions.get(handle)
    if (resource === undefined) {
      if (releasedHandleTombstone(resources, handle)) return alreadyReleasedCleanup()
      throw contractError('ownership.denied', 'ipc', 'electron-main-router.subscription-ownership')
    }
    const cleanup = await this.streams.removeSubscription(resources, handle, resource, true)
    return cleanupRecord(cleanup)
  }

  private async releaseDatabase(
    resources: RendererResources,
    payload: SerializableRecord
  ): Promise<SerializableRecord> {
    const handle = requiredString(payload, 'databaseHandle')
    const database = resources.databases.get(handle)
    if (database === undefined) return Object.freeze({ state: 'released', failures: [] })
    if (
      payload.connectionHandle !== undefined &&
      database.connectionHandle !== requiredString(payload, 'connectionHandle')
    ) {
      throw contractError('protocol.violation', 'ipc', 'electron-main-router.database-release-connection')
    }
    const cleanup = await this.releaseAcquiredWriters(resources, entry => entry.databaseHandle === handle)
    const failures = [...cleanup.failures]
    failures.push(...(await this.releaseOrderedGroups(resources, group => group.databaseHandle === handle)).failures)
    for (const [subscriptionHandle, subscription] of resources.subscriptions) {
      if (subscription.databaseHandle === handle)
        failures.push(
          ...(await this.streams.removeSubscription(resources, subscriptionHandle, subscription, true)).failures
        )
    }
    if (failures.length > 0) return cleanupRecord({ state: 'release-failed', failures })
    resources.databases.delete(handle)
    return Object.freeze({ state: 'released', failures: [] })
  }

  private cancel(resources: RendererResources, payload: SerializableRecord): SerializableRecord {
    const target = requiredString(payload, 'targetCorrelation')
    const now = this.pruneCancellationCorrelations(resources)
    const operation = resources.operations.get(target)
    if (operation !== undefined) {
      operation.controller.abort()
      return Object.freeze({ state: 'cancellation-requested' })
    }
    if (resources.preCancelledOperations.has(target)) {
      return Object.freeze({ state: 'cancellation-pending' })
    }
    if (resources.settledOperations.has(target)) {
      return Object.freeze({ state: 'already-terminal' })
    }
    if (resources.preCancelledOperations.size >= this.maximumOutstandingOperations) {
      throw contractError('stream.quota', 'ipc', 'electron-main-router.pre-cancellation-capacity')
    }
    resources.preCancelledOperations.set(target, now + CANCELLATION_CORRELATION_TTL_MILLISECONDS)
    return Object.freeze({ state: 'cancellation-pending' })
  }

  private database(resources: RendererResources, payload: SerializableRecord): ManagedDatabase {
    return requiredResource(resources.databases, requiredString(payload, 'databaseHandle'), 'database')
  }

  private characteristic(database: ManagedDatabase, payload: SerializableRecord): MainCharacteristicPath {
    return requiredResource(database.characteristics, requiredString(payload, 'characteristicHandle'), 'characteristic')
  }

  private descriptor(database: ManagedDatabase, payload: SerializableRecord): MainDescriptorPath {
    return requiredResource(database.descriptors, requiredString(payload, 'descriptorHandle'), 'descriptor')
  }

  private async releaseResources(clientId: string): Promise<CleanupRecord> {
    const key = clientId
    const resources = this.resources.get(key)
    if (resources === undefined) {
      return { state: 'released', failures: [] }
    }
    if (resources.lifecycle === 'releasing') {
      if (resources.releaseResult === null) {
        throw contractError('lifecycle.invariant-violation', 'ipc', 'electron-main-router.release-accounting')
      }
      return resources.releaseResult
    }
    resources.lifecycle = 'releasing'
    const releaseResult = this.releaseResourcesOnce(resources).then(
      cleanup => {
        if (cleanup.state === 'released') {
          this.resources.delete(key)
          this.rendererOwners.delete(key)
        } else {
          resources.lifecycle = 'active'
          resources.releaseResult = null
        }
        return cleanup
      },
      error => {
        console.error('[ElectronMainBleRouter] Renderer resource release rejected:', { clientId, error })
        resources.lifecycle = 'active'
        resources.releaseResult = null
        throw error
      }
    )
    resources.releaseResult = releaseResult
    return releaseResult
  }

  private async releaseResourcesOnce(resources: RendererResources): Promise<CleanupRecord> {
    if (resources.rollbackRetry !== null) {
      return {
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'operation-rollback',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron-main-router.rollback-pending')
              .normalized
          }
        ]
      }
    }
    for (const operation of resources.operations.values()) {
      operation.controller.abort()
    }
    const pendingOperations = [...resources.operations.values()]
    for (const operation of pendingOperations) {
      await operation.settled
    }
    const failures: CleanupFailure[] = []
    failures.push(...(await this.retainTerminalEvidenceForRecovery(resources)))
    failures.push(...(await this.releaseTerminalEvidenceRecoveryLease(resources)))
    this.orderedRecordings.retireUnused(resources.ownerKey, this.recordingLease(resources))
    this.orderedMarkers.retireUnusedForLease(this.recordingLease(resources))
    this.orderedRecovery.releaseLease(this.recordingLease(resources))
    failures.push(...(await this.releaseAcquiredWriters(resources)).failures)
    failures.push(...(await this.releaseOrderedGroups(resources, () => true, true)).failures)
    failures.push(...(await this.releaseReadinessWatches(resources)))
    failures.push(...(await this.releaseParameterWatches(resources)))
    for (const handle of resources.securityWatches.keys()) {
      try {
        const cleanup = await this.unwatchSecurity(resources, handle)
        if (cleanup.state !== 'released')
          failures.push({
            resourceKind: 'security-watch',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.security.watch-release').normalized
          })
      } catch (error) {
        failures.push({ resourceKind: 'security-watch', error: normalizedCleanupError(error) })
      }
    }
    for (const [handle, connectionEvents] of resources.connectionEventSubscriptions) {
      const cleanup = await this.connectionEvents.remove(resources, handle, connectionEvents, true)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      }
    }
    for (const [handle, subscription] of resources.subscriptions) {
      const cleanup = await this.streams.removeSubscription(resources, handle, subscription, false)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      }
    }
    for (const [handle, scan] of resources.scans) {
      const cleanup = await this.streams.stopScan(resources, handle, scan, false)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      }
    }
    for (const [handle, connection] of resources.connections) {
      if (this.hasDependentResourcesForConnection(resources, handle)) {
        continue
      }
      const cleanup = await this.disconnectConnection(resources, handle, connection)
      if (cleanup.state === 'released') {
        this.deleteDatabasesForConnection(resources, handle)
      } else {
        failures.push(...cleanup.failures)
      }
    }
    if (
      failures.length === 0 &&
      resources.securityWatches.size === 0 &&
      resources.readinessWatches.size === 0 &&
      resources.parameterWatches.size === 0 &&
      resources.pairingChallenges.size === 0 &&
      resources.scans.size === 0 &&
      resources.connectionEventSubscriptions.size === 0 &&
      resources.subscriptions.size === 0 &&
      resources.orderedGroups.size === 0 &&
      resources.acquiredWriters.size === 0 &&
      resources.connections.size === 0 &&
      resources.databases.size === 0
    ) {
      resources.pendingRollbacks.clear()
      return { state: 'released', failures: [] }
    }
    if (failures.length === 0) {
      failures.push({
        resourceKind: 'renderer-resources',
        error: contractError('lifecycle.invariant-violation', 'cleanup', 'electron-main-router.release-incomplete')
          .normalized
      })
    }
    return { state: 'release-failed', failures }
  }

  private async retainTerminalEvidenceForRecovery(resources: RendererResources): Promise<readonly CleanupFailure[]> {
    const failures: CleanupFailure[] = []
    for (const [handle, entry] of this.terminalEvidence.entries) {
      if (entry.ownerKey !== resources.ownerKey || entry.recoverable) continue
      if (entry.ref !== null) {
        // Commit retains the reservation's runtime authority until abandon.
        // Release it at renderer teardown so a same-process renderer reload
        // can recover the committed bytes (and create a later reservation),
        // while the durable ledger row remains for explicit recovery ACK.
        try {
          await entry.reservation.release()
          entry.recoverable = true
        } catch (error) {
          failures.push({ resourceKind: 'terminal-evidence', error: normalizedCleanupError(error) })
        }
        continue
      }
      try {
        await entry.reservation.release()
        this.terminalEvidence.entries.delete(handle)
      } catch (error) {
        failures.push({ resourceKind: 'terminal-evidence', error: normalizedCleanupError(error) })
      }
    }
    return failures
  }

  /** Drop only this renderer controller's native recovery authority. */
  private async releaseTerminalEvidenceRecoveryLease(resources: RendererResources): Promise<readonly CleanupFailure[]> {
    const backend = this.manager.attachedBackend.backend
    if (!(backend instanceof DesktopRustCoreBackend) || !backend.hasTerminalEvidenceRecovery()) return []
    try {
      await backend.releaseTerminalEvidenceRecoveryLease(resources.terminalEvidenceRecoveryLeaseId)
      for (const entry of this.terminalEvidence.entries.values()) {
        if (entry.recoveryLeaseId === resources.terminalEvidenceRecoveryLeaseId) entry.recoveryLeaseId = null
      }
      return []
    } catch (error) {
      return [{ resourceKind: 'terminal-evidence', error: normalizedCleanupError(error) }]
    }
  }

  /** Failed compensation remains owned independently of renderer admission.
   * A later request retries that exact debt; concurrent retries join one attempt. */
  private async retryOperationRollbacks(resources: RendererResources): Promise<void> {
    if (resources.rollbackRetry !== null) return resources.rollbackRetry
    if (resources.pendingRollbacks.size === 0) return
    const retry = async (): Promise<void> => {
      for (const [correlation, acquisitions] of resources.pendingRollbacks) {
        const result = await this.rollbackOperationResources(resources, acquisitions)
        if (result.state === 'released') resources.pendingRollbacks.delete(correlation)
      }
    }
    const attempt = retry()
    resources.rollbackRetry = attempt
    try {
      await attempt
    } finally {
      if (resources.rollbackRetry === attempt) resources.rollbackRetry = null
    }
  }

  private async rollbackOperationResources(
    resources: RendererResources,
    acquisitions: OperationAcquisitions
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    failures.push(
      ...(await this.releaseOrderedGroups(resources, (_group, handle) => acquisitions.orderedGroups.has(handle)))
        .failures
    )
    failures.push(
      ...(await this.releaseAcquiredWriters(resources, (_entry, handle) => acquisitions.acquiredWriters.has(handle)))
        .failures
    )
    for (const handle of resources.readinessWatches.keys()) {
      if (!acquisitions.readinessWatches.has(handle)) continue
      try {
        const cleanup = await this.unsubscribeWriteReadiness(resources, handle)
        if (cleanup.state !== 'released') {
          failures.push({
            resourceKind: 'gatt.write-readiness',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.write-readiness.rollback').normalized
          })
        }
      } catch (error) {
        failures.push({ resourceKind: 'gatt.write-readiness', error: normalizedCleanupError(error) })
      }
    }
    for (const handle of resources.parameterWatches.keys()) {
      if (!acquisitions.parameterWatches.has(handle)) continue
      try {
        const cleanup = await this.unsubscribeConnectionParameters(resources, handle)
        if (cleanup.state !== 'released') {
          failures.push({
            resourceKind: 'connection.parameters',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.connection-parameters.rollback')
              .normalized
          })
        }
      } catch (error) {
        failures.push({ resourceKind: 'connection.parameters', error: normalizedCleanupError(error) })
      }
    }
    for (const handle of resources.securityWatches.keys()) {
      if (!acquisitions.securityWatches.has(handle)) continue
      try {
        const cleanup = await this.unwatchSecurity(resources, handle)
        if (cleanup.state !== 'released')
          failures.push({
            resourceKind: 'security-watch',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.security.watch-rollback').normalized
          })
      } catch (error) {
        failures.push({ resourceKind: 'security-watch', error: normalizedCleanupError(error) })
      }
    }
    for (const [handle, connectionEvents] of resources.connectionEventSubscriptions) {
      if (!acquisitions.connectionEventSubscriptions.has(handle)) {
        continue
      }
      const cleanup = await this.connectionEvents.remove(resources, handle, connectionEvents, true)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
        console.error('[ElectronMainBleRouter] Connection lifecycle stream rollback failed after oversized response:', {
          handle,
          cleanup
        })
      }
    }
    for (const [handle, subscription] of resources.subscriptions) {
      if (!acquisitions.subscriptions.has(handle)) {
        continue
      }
      const cleanup = await this.streams.removeSubscription(resources, handle, subscription, false)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
        console.error('[ElectronMainBleRouter] Subscription rollback failed after oversized response:', {
          handle,
          cleanup
        })
      }
    }
    for (const [handle, scan] of resources.scans) {
      if (!acquisitions.scans.has(handle)) {
        continue
      }
      const cleanup = await this.streams.stopScan(resources, handle, scan, false)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
        console.error('[ElectronMainBleRouter] Scan rollback failed after oversized response:', { handle, cleanup })
      }
    }
    for (const [handle, connection] of resources.connections) {
      if (!acquisitions.connections.has(handle) || this.hasDependentResourcesForConnection(resources, handle)) {
        continue
      }
      const cleanup = await this.disconnectConnection(resources, handle, connection)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
        console.error('[ElectronMainBleRouter] Connection rollback failed after oversized response:', {
          handle,
          cleanup
        })
      }
    }
    for (const handle of resources.databases.keys()) {
      if (acquisitions.databases.has(handle)) {
        const cleanup = await this.releaseDatabase(resources, Object.freeze({ databaseHandle: handle }))
        if (cleanup.state !== 'released')
          failures.push({
            resourceKind: 'gatt-database',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.rollback.database-retained').normalized
          })
      }
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private async rollbackOperationAdmission<Renderer extends string, Operation extends string>(
    resources: RendererResources,
    acquisitions: OperationAcquisitions,
    envelope: IpcEnvelope<string, Renderer, Operation>
  ): Promise<CleanupRecord> {
    const rollback = await this.rollbackOperationResources(resources, acquisitions)
    if (envelope.command !== 'connection.events.ready') {
      return rollback
    }
    const handle = requiredString(envelope.payload, 'connectionEventsHandle')
    const resource = resources.connectionEventSubscriptions.get(handle)
    if (resource === undefined) {
      return rollback
    }
    const cleanup = await this.connectionEvents.remove(resources, handle, resource, true)
    if (cleanup.state === 'released') {
      return rollback
    }
    return { state: 'release-failed', failures: [...rollback.failures, ...cleanup.failures] }
  }

  private async disconnectConnection(
    resources: RendererResources,
    handle: string,
    connection: MainConnection
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    if (!(await this.cleanupResource('connection', () => connection.disconnect(), failures))) {
      return { state: 'release-failed', failures }
    }
    resources.connections.delete(handle)
    resources.releasedHandles.add(handle)
    return { state: 'released', failures: [] }
  }

  private async cleanupResource(
    resourceKind: string,
    cleanup: () => Promise<CleanupRecord>,
    failures: CleanupFailure[]
  ): Promise<boolean> {
    try {
      const result = await cleanup()
      failures.push(...result.failures)
      return result.state === 'released'
    } catch (error) {
      console.error('[ElectronMainBleRouter] Resource cleanup failed:', { resourceKind, error })
      failures.push({
        resourceKind,
        error: normalizedCleanupError(error)
      })
      return false
    }
  }

  private async releaseOrderedGroups(
    resources: RendererResources,
    include: (group: ManagedOrderedGroup, handle: string) => boolean = () => true,
    abandon = false
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const [handle, group] of resources.orderedGroups) {
      if (!include(group, handle)) continue
      try {
        if (abandon) {
          let receiptCount = 0
          for (const receipts of this.orphanReceipts.values()) receiptCount += receipts.size
          if (
            resources.ownerKey.length === 0 ||
            receiptCount + this.orphanReservations >= this.maximumOutstandingOperations
          ) {
            failures.push({
              resourceKind: 'ordered-orphan-capacity',
              error: contractError('stream.quota', 'cleanup', 'electron.ordered.orphan-capacity').normalized
            })
            continue
          }
          this.orphanReservations += 1
          try {
            await this.orderedMarkers.cancelAll(group.markerKey)
            const markerWrites = await this.orderedMarkers.snapshots(group.markerKey)
            const cleanup = await group.setup.stream.discard()
            if (
              cleanup.group.state === 'release-failed' ||
              cleanup.members.some(member => member.cleanup.state === 'release-failed')
            ) {
              failures.push(...cleanup.group.failures)
              for (const member of cleanup.members) failures.push(...member.cleanup.failures)
              continue
            }
            await this.orderedMarkers.forgetAll(group.markerKey)
            const retained = [
              ...(group.delivery === null ? [] : await group.delivery.snapshotRetained()),
              ...(group.pendingSetupObservations ?? [])
            ]
            const local = accountOrderedDiscard(retained)
            const native = cleanup.discarded ?? {
              values: '0',
              bytes: '0',
              controls: '0',
              uncertainTail: true,
              terminal: null,
              losses: []
            }
            const receiptId = this.allocateHandle('ordered-orphan')
            const receipt = orderedWireRecord({
              format: 'public',
              receiptVersion: 2,
              receiptId,
              values: String(BigInt(native.values) + BigInt(local.values)),
              bytes: String(BigInt(native.bytes) + BigInt(local.bytes)),
              controls: String(BigInt(native.controls) + BigInt(local.controls)),
              uncertainTail: native.uncertainTail || retained.length > 0,
              terminal: local.terminal ?? native.terminal ?? null,
              losses: [...(local.losses ?? []), ...(native.losses ?? [])],
              failureObservations: [...(local.failureObservations ?? []), ...(native.failureObservations ?? [])],
              failureObservationsOmitted: [
                ...(local.failureObservationsOmitted ?? []),
                ...(native.failureObservationsOmitted ?? [])
              ],
              markers: [...(local.markers ?? []), ...(native.markers ?? [])],
              markerWrites,
              trustedValues: native.trustedValues ?? []
            })
            const receipts = this.orphanReceipts.get(resources.ownerKey) ?? new Map<string, ManagedOrphanReceipt>()
            receipts.set(receiptId, {
              record: receipt,
              scalars: Array.from(JSON.stringify(receipt)),
              cursor: 0,
              nextOffset: null,
              page: null,
              complete: false
            })
            this.orphanReceipts.set(resources.ownerKey, receipts)
            group.delivery?.commitAbandonment()
            group.pendingSetupObservations?.splice(0)
            resources.orderedGroups.delete(handle)
            resources.releasedHandles.add(handle)
          } finally {
            this.orphanReservations -= 1
          }
          continue
        }
        const cleanup = await (group.setup.state === 'ready' ? group.setup.stream.close() : group.setup.close())
        failures.push(...cleanup.group.failures)
        for (const member of cleanup.members) failures.push(...member.cleanup.failures)
        group.cleanupReleased =
          cleanup.group.state === 'released' && cleanup.members.every(member => member.cleanup.state === 'released')
        if (
          group.cleanupReleased &&
          (group.delivery === null || group.delivery.drained) &&
          !this.orderedMarkers.hasActive(group.markerKey)
        ) {
          resources.orderedGroups.delete(handle)
          this.orderedMarkers.releaseOwner(group.markerKey)
          resources.releasedHandles.add(handle)
        } else if (group.cleanupReleased) {
          failures.push({
            resourceKind: 'ordered-group-uncertain-tail',
            error: contractError('lifecycle.invalid-state', 'cleanup', 'electron.ordered.undrained').normalized
          })
        }
      } catch (error) {
        failures.push({ resourceKind: 'ordered-group', error: normalizedCleanupError(error) })
      }
    }
    return { state: failures.length === 0 ? 'released' : 'release-failed', failures }
  }

  private async releaseSubscriptionsForConnection(
    resources: RendererResources,
    connectionHandle: string
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const [handle, resource] of resources.subscriptions) {
      if (!this.isSubscriptionForConnection(resources, resource, connectionHandle)) {
        continue
      }
      const cleanup = await this.streams.removeSubscription(resources, handle, resource, true)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      }
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private async releaseConnectionEventSubscriptionsForConnection(
    resources: RendererResources,
    connectionHandle: string
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const [handle, resource] of resources.connectionEventSubscriptions) {
      if (resource.connectionHandle !== connectionHandle) {
        continue
      }
      const cleanup = await this.connectionEvents.remove(resources, handle, resource, true)
      if (cleanup.state === 'release-failed') {
        failures.push(...cleanup.failures)
      }
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private isSubscriptionForConnection(
    resources: RendererResources,
    subscription: ManagedSubscription,
    connectionHandle: string
  ): boolean {
    return resources.databases.get(subscription.databaseHandle)?.connectionHandle === connectionHandle
  }

  private hasDependentResourcesForConnection(resources: RendererResources, connectionHandle: string): boolean {
    for (const entry of resources.acquiredWriters.values()) {
      if (entry.connectionHandle === connectionHandle) return true
    }
    for (const group of resources.orderedGroups.values()) {
      if (resources.databases.get(group.databaseHandle)?.connectionHandle === connectionHandle) return true
    }
    for (const resource of resources.connectionEventSubscriptions.values()) {
      if (resource.connectionHandle === connectionHandle) {
        return true
      }
    }
    for (const subscription of resources.subscriptions.values()) {
      if (this.isSubscriptionForConnection(resources, subscription, connectionHandle)) {
        return true
      }
    }
    return false
  }

  private deleteDatabasesForConnection(resources: RendererResources, connectionHandle: string): void {
    for (const [databaseHandle, database] of resources.databases) {
      if (database.connectionHandle === connectionHandle) {
        resources.databases.delete(databaseHandle)
      }
    }
  }

  private async retireDatabasesForRediscovery(
    resources: RendererResources,
    connectionHandle: string
  ): Promise<CleanupRecord> {
    const failures: CleanupFailure[] = []
    for (const [databaseHandle, database] of resources.databases) {
      if (database.connectionHandle !== connectionHandle) {
        continue
      }
      const writerCleanup = await this.releaseAcquiredWriters(
        resources,
        entry => entry.databaseHandle === databaseHandle
      )
      failures.push(...writerCleanup.failures)
      const orderedCleanup = await this.releaseOrderedGroups(
        resources,
        group => group.databaseHandle === databaseHandle
      )
      failures.push(...orderedCleanup.failures)
      let retired = writerCleanup.state === 'released' && orderedCleanup.state === 'released'
      for (const [subscriptionHandle, subscription] of resources.subscriptions) {
        if (subscription.databaseHandle !== databaseHandle) {
          continue
        }
        const cleanup = await this.streams.removeSubscription(resources, subscriptionHandle, subscription, true)
        if (cleanup.state === 'release-failed') {
          retired = false
          failures.push(...cleanup.failures)
        }
      }
      if (retired) {
        resources.databases.delete(databaseHandle)
      }
    }
    return failures.length === 0 ? { state: 'released', failures: [] } : { state: 'release-failed', failures }
  }

  private resourcesFor(rendererLease: RendererLeaseIdentity): RendererResources {
    const key = String(rendererLease.leaseId)
    const existing = this.resources.get(key)
    if (existing !== undefined) {
      return existing
    }
    const resources: RendererResources = {
      securityWatches: new Map(),
      readinessWatches: new Map(),
      parameterWatches: new Map(),
      pairingChallenges: new Map(),
      rendererLease,
      ownerKey: this.rendererOwners.get(String(rendererLease.leaseId)) ?? '',
      terminalEvidenceRecoveryLeaseId: `electron-terminal-evidence-recovery-${this.registryScope}-${this.nextTerminalEvidenceRecoveryLease++}`,
      scans: new Map(),
      connections: new Map(),
      connectionEventSubscriptions: new Map(),
      databases: new Map(),
      subscriptions: new Map(),
      orderedGroups: new Map(),
      releasedOrderedGroups: new Map(),
      forgottenOrderedGroups: new Set(),
      acquiredWriters: new Map(),
      reservedHandles: new Set(),
      pendingRollbacks: new Map(),
      rollbackRetry: null,
      operations: new Map(),
      preCancelledOperations: new Map(),
      settledOperations: new Map(),
      releasedHandles: new Set(),
      peerFailureRecoveries: new Map(),
      peerFailureRecoveryGranted: false,
      scanFailureRecoveries: new Map(),
      scanFailureHandles: new Map(),
      scanFailureRecoveryGranted: false,
      lifecycle: 'active',
      releaseResult: null
    }
    this.resources.set(key, resources)
    return resources
  }

  private grantDurablePeerFailureRecovery(resources: RendererResources): void {
    const backend = this.manager.attachedBackend.backend
    if (!(backend instanceof DesktopRustCoreBackend)) {
      throw contractError(
        'capability.unsupported',
        'restoration',
        'electron.peer-failure-recovery.desktop-core-required'
      )
    }
    for (const recovery of backend.durablePeerNotificationFailureRecoveries()) {
      resources.peerFailureRecoveries.set(`peer-failure-recovery-${this.nextHandle++}`, recovery)
    }
  }

  private assertPeerFailureRecoveryProtocol(envelope: IpcEnvelope<string, string, string>): void {
    if (envelope.versions.ipcProtocol.selected.value < 10) {
      throw contractError('capability.unsupported', 'restoration', 'electron.peer-failure-recovery.ipc-v10-required')
    }
  }

  private assertOrderedRouteProtocol(envelope: IpcEnvelope<string, string, string>): void {
    if (
      isOrderedRouteCommand(envelope.command) &&
      envelope.versions.ipcProtocol.selected.value < ORDERED_ROUTE_MINIMUM_IPC_PROTOCOL
    ) {
      throw contractError('capability.unsupported', 'gatt', 'electron.ordered.ipc-v9-required')
    }
  }

  private listPeerFailureRecovery(resources: RendererResources): SerializableRecord {
    if (resources.peerFailureRecoveries.size === 0) {
      throw contractError('ownership.denied', 'restoration', 'electron.peer-failure-recovery.not-granted')
    }
    return Object.freeze({
      recoveries: Object.freeze(
        [...resources.peerFailureRecoveries.entries()].map(([handleId, recovery]) => {
          const characteristicUuid = recovery.provenance.selector.characteristicUuid
          if (characteristicUuid === undefined)
            throw contractError('protocol.malformed', 'restoration', 'electron.peer-failure-recovery.selector')
          return Object.freeze({
            handleId,
            peerId: recovery.provenance.peerId,
            selector: Object.freeze({
              serviceUuid: recovery.provenance.selector.serviceUuid,
              serviceOccurrence: String(recovery.provenance.selector.serviceOccurrence),
              characteristicUuid,
              characteristicOccurrence: String(recovery.provenance.selector.characteristicOccurrence)
            }),
            contiguousRead: recovery.provenance.contiguousRead,
            acknowledged: recovery.provenance.acknowledged,
            producerAck: recovery.provenance.producerAck
          })
        })
      )
    })
  }

  private async readPeerFailureRecovery(
    resources: RendererResources,
    payload: SerializableRecord
  ): Promise<SerializableRecord> {
    const handleId = requiredString(payload, 'handleId')
    const offset = requiredString(payload, 'offset')
    if (!/^(?:0|[1-9][0-9]*)$/.test(offset))
      throw contractError('argument.invalid', 'restoration', 'electron.peer-failure-recovery.offset')
    const recovery = requiredResource(resources.peerFailureRecoveries, handleId, 'peer-failure-recovery')
    const result = await recovery.read(offset)
    if (resources.lifecycle !== 'active' || resources.peerFailureRecoveries.get(handleId) !== recovery)
      throw contractError('ownership.denied', 'restoration', 'electron.peer-failure-recovery.revoked')
    return Object.freeze({
      offset: result.offset,
      nextOffset: result.nextOffset,
      bytes: ownBytes(result.bytes, byteLimit(this.maximumMessageBytes)),
      done: result.done
    })
  }

  private async acknowledgePeerFailureRecovery(
    resources: RendererResources,
    payload: SerializableRecord
  ): Promise<SerializableRecord> {
    const handleId = requiredString(payload, 'handleId')
    const recovery = requiredResource(resources.peerFailureRecoveries, handleId, 'peer-failure-recovery')
    await recovery.acknowledge()
    if (resources.lifecycle !== 'active' || resources.peerFailureRecoveries.get(handleId) !== recovery)
      throw contractError('ownership.denied', 'restoration', 'electron.peer-failure-recovery.revoked')
    resources.peerFailureRecoveries.delete(handleId)
    return Object.freeze({ acknowledged: true })
  }

  private assertScanFailureRecoveryProtocol(envelope: IpcEnvelope<string, string, string>): void {
    if (envelope.versions.ipcProtocol.selected.value < 10)
      throw contractError('capability.unsupported', 'restoration', 'electron.scan-failure-recovery.ipc-v10-required')
  }

  private assertTerminalEvidenceProtocol(envelope: IpcEnvelope<string, string, string>): void {
    if (envelope.versions.ipcProtocol.selected.value < 11)
      throw contractError('capability.unsupported', 'restoration', 'electron.terminal-evidence.ipc-v11-required')
  }

  private scanRecovery(resources: RendererResources, handleId: string): DesktopScanFailureRecovery {
    const recovery = requiredResource(resources.scanFailureRecoveries, handleId, 'scan-failure-recovery')
    if (resources.lifecycle !== 'active' || resources.scanFailureRecoveries.get(handleId) !== recovery)
      throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
    return recovery
  }

  private scanHandle(
    resources: RendererResources,
    handleId: string,
    kind: 'owner' | 'head' | 'cleanup'
  ): { readonly recovery: DesktopScanFailureRecovery; readonly nativeHandle: string } {
    const handle = requiredResource(resources.scanFailureHandles, handleId, 'scan-failure-recovery-handle')
    if (
      handle.kind !== kind ||
      resources.lifecycle !== 'active' ||
      resources.scanFailureHandles.get(handleId) !== handle
    )
      throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
    return handle
  }

  private bindScanHandle(
    resources: RendererResources,
    recovery: DesktopScanFailureRecovery,
    nativeHandle: string,
    kind: 'owner' | 'head' | 'cleanup'
  ): string {
    for (const [handleId, handle] of resources.scanFailureHandles) {
      if (handle.recovery === recovery && handle.nativeHandle === nativeHandle && handle.kind === kind) return handleId
    }
    const handleId = `scan-failure-${kind}-${this.nextHandle++}`
    resources.scanFailureHandles.set(handleId, Object.freeze({ recovery, nativeHandle, kind }))
    return handleId
  }

  private async routeScanFailureRecovery(
    resources: RendererResources,
    command: string,
    payload: SerializableRecord
  ): Promise<SerializableRecord> {
    if (!resources.scanFailureRecoveryGranted)
      throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.not-granted')
    if (command === 'scanFailureRecovery.list') {
      return Object.freeze({
        recoveries: Object.freeze(
          [...resources.scanFailureRecoveries.keys()].map(handleId => Object.freeze({ handleId }))
        )
      })
    }
    const recoveryId = requiredString(payload, 'recoveryId')
    const recovery = this.scanRecovery(resources, recoveryId)
    if (command === 'scanFailureRecovery.owners') {
      const owners = await recovery.owners()
      if (resources.lifecycle !== 'active' || resources.scanFailureRecoveries.get(recoveryId) !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({
        owners: Object.freeze(
          owners.map(owner =>
            Object.freeze({
              handleId: this.bindScanHandle(resources, recovery, owner.handleId, 'owner'),
              prepared: owner.prepared,
              producerFinished: owner.producerFinished,
              rawRetired: owner.rawRetired,
              rawReleased: owner.rawReleased,
              memberCount: owner.memberCount,
              released: owner.released
            })
          )
        )
      })
    }
    if (command === 'scanFailureRecovery.cleanup.list') {
      const receipts = await recovery.pendingCleanupReceipts()
      if (resources.lifecycle !== 'active' || resources.scanFailureRecoveries.get(recoveryId) !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({
        cleanupReceiptIds: Object.freeze(
          receipts.map(native => this.bindScanHandle(resources, recovery, native, 'cleanup'))
        )
      })
    }
    const handleId = requiredString(payload, 'handleId')
    if (command === 'scanFailureRecovery.member' || command === 'scanFailureRecovery.head') {
      const owner = this.scanHandle(resources, handleId, 'owner')
      if (owner.recovery !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.owner')
      const indexValue = payload.index
      if (typeof indexValue !== 'number' || !Number.isSafeInteger(indexValue) || indexValue < 0)
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.index')
      const index = indexValue
      if (command === 'scanFailureRecovery.member') {
        const member = await recovery.member(owner.nativeHandle, index)
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ member: Object.freeze({ recipient: member.recipient, delivery: member.delivery }) })
      }
      const head = await recovery.head(owner.nativeHandle, index)
      if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({
        head:
          head === null
            ? null
            : Object.freeze({
                handleId: this.bindScanHandle(resources, recovery, head.handleId, 'head'),
                contiguousRead: head.contiguousRead,
                acknowledged: head.acknowledged
              })
      })
    }
    if (
      command === 'scanFailureRecovery.raw.info' ||
      command === 'scanFailureRecovery.raw.member' ||
      command === 'scanFailureRecovery.raw.token' ||
      command === 'scanFailureRecovery.raw.retire' ||
      command === 'scanFailureRecovery.raw.release' ||
      command === 'scanFailureRecovery.raw.finalize'
    ) {
      const owner = this.scanHandle(resources, handleId, 'owner')
      if (owner.recovery !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.owner')
      const wholeNumber = (name: string): number => {
        const value = payload[name]
        if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0)
          throw contractError('argument.invalid', 'restoration', `electron.scan-failure-recovery.raw.${name}`)
        return value
      }
      if (command === 'scanFailureRecovery.raw.info') {
        const info = await recovery.rawInfo(owner.nativeHandle)
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ info })
      }
      if (command === 'scanFailureRecovery.raw.member') {
        const member = await recovery.rawMember(owner.nativeHandle, wholeNumber('index'))
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ member })
      }
      if (command === 'scanFailureRecovery.raw.token') {
        const maxBytes = wholeNumber('maxBytes')
        if (maxBytes < 1 || maxBytes > 65536 || maxBytes > this.maximumMessageBytes)
          throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.raw-token.max-bytes')
        const token = await recovery.rawToken(owner.nativeHandle, wholeNumber('index'), wholeNumber('offset'), maxBytes)
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ token })
      }
      if (command === 'scanFailureRecovery.raw.retire') await recovery.rawRetire(owner.nativeHandle)
      else if (command === 'scanFailureRecovery.raw.release') await recovery.rawRelease(owner.nativeHandle)
      else {
        const nativeReceipt = await recovery.rawFinalize(owner.nativeHandle)
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ cleanupReceiptId: this.bindScanHandle(resources, recovery, nativeReceipt, 'cleanup') })
      }
      if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({ completed: true })
    }
    if (command === 'scanFailureRecovery.read') {
      const head = this.scanHandle(resources, handleId, 'head')
      if (head.recovery !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.head')
      const offset = requiredString(payload, 'offset')
      const page = await recovery.read(head.nativeHandle, offset)
      if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({
        offset: page.offset,
        nextOffset: page.nextOffset,
        bytes: ownBytes(page.bytes, byteLimit(this.maximumMessageBytes)),
        done: page.done
      })
    }
    if (
      command === 'scanFailureRecovery.ack' ||
      command === 'scanFailureRecovery.retire' ||
      command === 'scanFailureRecovery.release' ||
      command === 'scanFailureRecovery.finalize'
    ) {
      const head = this.scanHandle(resources, handleId, 'head')
      if (head.recovery !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.head')
      if (command === 'scanFailureRecovery.ack') await recovery.acknowledge(head.nativeHandle)
      else if (command === 'scanFailureRecovery.retire') await recovery.retire(head.nativeHandle)
      else if (command === 'scanFailureRecovery.release') await recovery.release(head.nativeHandle)
      else {
        const nativeReceipt = await recovery.finalizeRelease(head.nativeHandle)
        if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
          throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
        return Object.freeze({ cleanupReceiptId: this.bindScanHandle(resources, recovery, nativeReceipt, 'cleanup') })
      }
      if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      return Object.freeze({ acknowledged: command === 'scanFailureRecovery.ack', completed: true })
    }
    if (command === 'scanFailureRecovery.cleanup.retire') {
      const cleanup = this.scanHandle(resources, handleId, 'cleanup')
      if (cleanup.recovery !== recovery)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.cleanup')
      await recovery.retireCleanup(cleanup.nativeHandle)
      if (resources.lifecycle !== 'active' || resources.scanFailureHandles.get(handleId) === undefined)
        throw contractError('ownership.denied', 'restoration', 'electron.scan-failure-recovery.revoked')
      resources.scanFailureHandles.delete(handleId)
      return Object.freeze({ retired: true })
    }
    throw contractError('protocol.violation', 'ipc', 'electron.scan-failure-recovery.command')
  }

  private async routeTerminalEvidence(
    resources: RendererResources,
    command: string,
    payload: SerializableRecord,
    binaryPayload: OwnedBytes | null
  ): Promise<SerializableRecord> {
    if (command === 'terminalEvidence.reserve') {
      if (binaryPayload !== null) throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.reserve')
      const request = terminalEvidenceReservationRequest(payload)
      const source = this.terminalEvidenceSource(request.producer)
      const reservation = await source.reserveTerminalEvidence(request)
      const releaseRevokedReservation = async (): Promise<never> => {
        await reservation.release()
        throw contractError('ownership.denied', 'restoration', 'electron.terminal-evidence.reserve.revoked')
      }
      if (resources.lifecycle !== 'active') await releaseRevokedReservation()
      const backend = this.manager.attachedBackend.backend
      if (backend instanceof DesktopRustCoreBackend) {
        try {
          await backend.bindTerminalEvidenceRecipientForRecovery(request.producer, request.scope, resources.ownerKey)
        } catch (bindingError) {
          try {
            await reservation.release()
          } catch (releaseError) {
            throw new AggregateError(
              [bindingError, releaseError],
              'Electron terminal-evidence recipient binding and reservation release both failed'
            )
          }
          throw bindingError
        }
      }
      // Binding crosses an asynchronous native boundary. Renderer teardown can
      // complete while it awaits, before this provisional reservation reaches
      // the process registry; never publish such an orphaned row.
      if (resources.lifecycle !== 'active') await releaseRevokedReservation()
      const reservationId = `terminal-evidence-${this.terminalEvidence.nextId++}`
      this.terminalEvidence.entries.set(reservationId, {
        ownerKey: resources.ownerKey,
        recoveryLeaseId: null,
        producer: request.producer,
        scope: request.scope,
        source,
        reservation,
        ref: null,
        digest: null,
        byteLength: null,
        recoverable: false
      })
      return Object.freeze({ kind: 'reserved', reservationId, maximumBytes: reservation.maximumBytes })
    }
    if (command === 'terminalEvidence.commit') {
      const request = terminalEvidenceCommitRequest(payload, binaryPayload)
      const entry = this.terminalEvidenceOwner(resources, request.reservationId, 'electron.terminal-evidence.commit')
      if (entry.ref !== null)
        throw contractError('lifecycle.invalid-state', 'restoration', 'electron.terminal-evidence.commit')
      const backend = this.manager.attachedBackend.backend
      if (backend instanceof DesktopRustCoreBackend) {
        await backend.stageTerminalEvidenceForRecovery(
          entry.producer,
          entry.scope,
          resources.ownerKey,
          request.canonicalBytes
        )
      }
      const result = await entry.reservation.commit({ canonicalBytes: request.canonicalBytes, digest: request.digest })
      if ('kind' in result) return Object.freeze({ kind: 'refused', reason: result.reason })
      entry.ref = result.ref
      entry.digest = result.digest
      entry.byteLength = result.byteLength
      this.terminalEvidence.entries.set(result.ref, entry)
      return Object.freeze({
        kind: 'committed',
        ref: result.ref,
        byteLength: String(result.byteLength),
        digest: result.digest,
        formatVersion: 1,
        durability: 'committed'
      })
    }
    if (command === 'terminalEvidence.read') {
      if (binaryPayload !== null) throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.read')
      const ref = terminalEvidenceReference(payload, 'electron.terminal-evidence.read')
      const entry = this.terminalEvidenceOwner(resources, ref, 'electron.terminal-evidence.read')
      if (entry.ref === null) throw contractError('ownership.denied', 'restoration', 'electron.terminal-evidence.read')
      const result = await entry.source.readTerminalEvidence(entry.ref, entry.byteLength ?? 0)
      if (result.kind !== 'committed') return Object.freeze({ kind: result.kind })
      return Object.freeze({
        kind: 'committed',
        canonicalBytes: ownBytes(result.canonicalBytes, byteLimit(result.canonicalBytes.byteLength)),
        digest: result.digest
      })
    }
    if (command === 'terminalEvidence.release') {
      if (binaryPayload !== null) throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.release')
      const reservationId = terminalEvidenceReference(payload, 'electron.terminal-evidence.release')
      const entry = this.terminalEvidenceOwner(resources, reservationId, 'electron.terminal-evidence.release')
      await entry.reservation.release()
      // Stream drainage abandons only the runtime lease. Exact staged or
      // committed bytes remain owned by recovery and keep their quota until
      // the authenticated recovery ACK below succeeds.
      if (entry.ref === null) {
        this.terminalEvidence.entries.delete(reservationId)
      } else {
        entry.recoverable = true
      }
      return Object.freeze({ kind: 'released' })
    }
    if (command === 'terminalEvidence.recovery.list') {
      terminalEvidenceRecoveryList(payload, binaryPayload)
      const backend = this.manager.attachedBackend.backend
      if (backend instanceof DesktopRustCoreBackend && backend.hasTerminalEvidenceRecovery()) {
        // Preserve the legacy strict list contract: a producer lane held by
        // another controller remains an explicit Busy failure, never an empty
        // recovery result. Bootstrap uses the additive status path so the
        // renderer can subsequently choose that richer result.
        const recovered = await backend.recoverTerminalEvidenceForRecipient(
          resources.ownerKey,
          resources.terminalEvidenceRecoveryLeaseId
        )
        for (const record of recovered) this.bindTerminalEvidenceRecovery(resources, record)
      } else {
        this.bindLocalTerminalEvidenceRecovery(resources)
      }
      const records = [...this.terminalEvidence.entries.entries()]
        .filter(
          ([handle, entry]) =>
            handle !== entry.ref &&
            entry.ownerKey === resources.ownerKey &&
            entry.recoveryLeaseId === resources.terminalEvidenceRecoveryLeaseId &&
            entry.recoverable &&
            entry.ref !== null
        )
        .map(([handle, entry]) =>
          Object.freeze({
            handle,
            producer: entry.producer,
            byteLength: entry.byteLength,
            digest: entry.digest,
            formatVersion: 1
          })
        )
      return Object.freeze({ records: Object.freeze(records) })
    }
    if (command === 'terminalEvidence.recovery.listWithStatus') {
      terminalEvidenceRecoveryList(payload, binaryPayload)
      const backend = this.manager.attachedBackend.backend
      let busyProducers: readonly ('scan' | 'subscription')[] = []
      if (backend instanceof DesktopRustCoreBackend && backend.hasTerminalEvidenceRecovery()) {
        const refreshed = await backend.recoverTerminalEvidenceForRecipientWithStatus(
          resources.ownerKey,
          resources.terminalEvidenceRecoveryLeaseId
        )
        busyProducers = refreshed.busyProducers
        for (const record of refreshed.records) this.bindTerminalEvidenceRecovery(resources, record)
      } else {
        this.bindLocalTerminalEvidenceRecovery(resources)
      }
      const records = [...this.terminalEvidence.entries.entries()]
        .filter(
          ([handle, entry]) =>
            handle !== entry.ref &&
            entry.ownerKey === resources.ownerKey &&
            entry.recoveryLeaseId === resources.terminalEvidenceRecoveryLeaseId &&
            entry.recoverable &&
            entry.ref !== null
        )
        .map(([handle, entry]) =>
          Object.freeze({
            handle,
            producer: entry.producer,
            byteLength: entry.byteLength,
            digest: entry.digest,
            formatVersion: 1
          })
        )
      return Object.freeze({ records: Object.freeze(records), busyProducers: Object.freeze(busyProducers) })
    }
    if (command === 'terminalEvidence.recovery.read') {
      const request = terminalEvidenceRecoveryRead(payload, binaryPayload)
      const entry = this.terminalEvidenceRecoveryOwner(
        resources,
        request.handle,
        'electron.terminal-evidence.recovery.read'
      )
      const result = await entry.source.readTerminalEvidence(entry.ref!, request.maximumBytes)
      if (result.kind !== 'committed')
        throw contractError('platform.failure', 'restoration', 'electron.terminal-evidence.recovery.read')
      return Object.freeze({
        kind: 'committed',
        canonicalBytes: ownBytes(result.canonicalBytes, byteLimit(result.canonicalBytes.byteLength)),
        digest: result.digest
      })
    }
    if (command === 'terminalEvidence.recovery.ack') {
      const handle = terminalEvidenceRecoveryAck(payload, binaryPayload)
      const entry = this.terminalEvidenceRecoveryOwner(resources, handle, 'electron.terminal-evidence.recovery.ack')
      await entry.source.releaseTerminalEvidence(entry.ref!)
      this.terminalEvidence.entries.delete(handle)
      this.terminalEvidence.entries.delete(entry.ref!)
      return Object.freeze({ state: 'released' })
    }
    throw contractError('argument.invalid', 'ipc', 'electron.terminal-evidence.command')
  }

  private terminalEvidenceSource(producer: 'scan' | 'subscription'): ElectronTerminalEvidenceSource {
    const backend = this.manager.attachedBackend.backend
    const source = producer === 'scan' ? backend.scanner : backend.gatt
    if (
      source.reserveTerminalEvidence === undefined ||
      source.readTerminalEvidence === undefined ||
      source.releaseTerminalEvidence === undefined
    ) {
      throw contractError('capability.unsupported', 'restoration', 'electron.terminal-evidence.source')
    }
    return Object.freeze({
      reserveTerminalEvidence: source.reserveTerminalEvidence,
      readTerminalEvidence: source.readTerminalEvidence,
      releaseTerminalEvidence: source.releaseTerminalEvidence
    })
  }

  private async hydrateTerminalEvidenceRecovery(resources: RendererResources): Promise<void> {
    const backend = this.manager.attachedBackend.backend
    if (!(backend instanceof DesktopRustCoreBackend)) return
    // A host without a configured ledger has no retained evidence to recover.
    // Bootstrap must not turn another live controller's authority into a
    // renderer-start failure. The status path restores cold records and
    // preserves contended lanes for the explicit public status query.
    if (!backend.hasTerminalEvidenceRecovery()) return
    const recovered = await backend.recoverTerminalEvidenceForRecipientWithStatus(
      resources.ownerKey,
      resources.terminalEvidenceRecoveryLeaseId
    )
    for (const record of recovered.records) this.bindTerminalEvidenceRecovery(resources, record)
  }

  /**
   * Attaches a recovered row to exactly the controller that selected it. A
   * process-wide entry is retained for durable bytes, but its renderer alias
   * is never process-wide: a new controller revokes a stale controller's
   * ability to read or acknowledge the same reference.
   */
  private bindTerminalEvidenceRecovery(
    resources: RendererResources,
    record: {
      readonly producer: 'scan' | 'subscription'
      readonly reservationId: string
      readonly reference: string
      readonly digest: string
      readonly byteLength: number
    }
  ): void {
    const existing = [...this.terminalEvidence.entries.values()].find(entry => entry.ref === record.reference)
    if (existing !== undefined) {
      if (existing.ownerKey !== resources.ownerKey)
        throw contractError('ownership.denied', 'restoration', 'electron.terminal-evidence.recovery-owner')
      existing.recoveryLeaseId = resources.terminalEvidenceRecoveryLeaseId
      existing.recoverable = true
      return
    }
    const source = this.terminalEvidenceSource(record.producer)
    const reservation: TerminalEvidenceReservation = Object.freeze({
      maximumBytes: record.byteLength,
      commit: async () => {
        throw contractError('lifecycle.invalid-state', 'restoration', 'electron.terminal-evidence.recovered-commit')
      },
      release: async () => undefined
    })
    this.terminalEvidence.entries.set(`terminal-evidence-recovery-${this.terminalEvidence.nextId++}`, {
      ownerKey: resources.ownerKey,
      recoveryLeaseId: resources.terminalEvidenceRecoveryLeaseId,
      producer: record.producer,
      scope: null,
      source,
      reservation,
      ref: record.reference,
      digest: record.digest,
      byteLength: record.byteLength,
      recoverable: true
    })
  }

  /** Test and non-durable hosts still fence in-process recovery aliases by lease. */
  private bindLocalTerminalEvidenceRecovery(resources: RendererResources): void {
    for (const entry of this.terminalEvidence.entries.values()) {
      if (entry.ownerKey === resources.ownerKey && entry.recoverable && entry.ref !== null)
        entry.recoveryLeaseId = resources.terminalEvidenceRecoveryLeaseId
    }
  }

  private terminalEvidenceOwner(
    resources: RendererResources,
    handle: string,
    operation: string
  ): ElectronTerminalEvidenceEntry {
    const entry = this.terminalEvidence.entries.get(handle)
    if (entry === undefined || entry.ownerKey !== resources.ownerKey)
      throw contractError('ownership.denied', 'restoration', operation)
    return entry
  }

  private terminalEvidenceRecoveryOwner(
    resources: RendererResources,
    handle: string,
    operation: string
  ): ElectronTerminalEvidenceEntry {
    const entry = this.terminalEvidenceOwner(resources, handle, operation)
    if (!entry.recoverable || entry.ref === null || entry.recoveryLeaseId !== resources.terminalEvidenceRecoveryLeaseId)
      throw contractError('ownership.denied', 'restoration', operation)
    return entry
  }

  private allocateHandle(
    kind:
      | 'scan'
      | 'connection'
      | 'characteristic'
      | 'descriptor'
      | 'database'
      | 'subscription'
      | 'acquired-writer'
      | 'ordered-orphan'
  ): string {
    for (;;) {
      const handle = `${kind}-${this.nextHandle++}`
      if (!this.isAllocatedHandle(handle)) {
        return handle
      }
    }
  }

  private isAllocatedHandle(handle: string): boolean {
    if (isElectronConnectionEventsStreamHandle(handle)) {
      return true
    }
    for (const resources of this.resources.values()) {
      if (
        resources.scans.has(handle) ||
        resources.connections.has(handle) ||
        resources.connectionEventSubscriptions.has(handle) ||
        resources.databases.has(handle) ||
        resources.subscriptions.has(handle) ||
        resources.acquiredWriters.has(handle)
      ) {
        return true
      }
    }
    return false
  }

  private pruneCancellationCorrelations(resources: RendererResources): number {
    const now = this.cancellationClock()
    pruneExpiredCorrelationRecords(resources.preCancelledOperations, now)
    pruneExpiredCorrelationRecords(resources.settledOperations, now)
    trimCorrelationRecords(resources.settledOperations, this.maximumOutstandingOperations)
    return now
  }

  private recordSettledOperation(resources: RendererResources, operationKey: string): void {
    const now = this.pruneCancellationCorrelations(resources)
    resources.settledOperations.set(operationKey, now + CANCELLATION_CORRELATION_TTL_MILLISECONDS)
    trimCorrelationRecords(resources.settledOperations, this.maximumOutstandingOperations)
  }

  private event(rendererLease: RendererLeaseIdentity, streamId: string, item: SerializableRecord): ElectronBleIpcEvent {
    return Object.freeze({
      rendererLease,
      eventId: `event-${this.nextEvent++}`,
      streamId,
      item
    })
  }
}

function createElectronHostIpcVersionAxes(core: HostNeutralBackendIdentity<string>['versions']): IpcVersionAxes {
  const coreOffer = coreCompatibilityOffer(core)
  const ipcOffer = IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol
  return Object.freeze({
    ...negotiateCoreVersions(coreOffer, coreOffer),
    ipcProtocol: negotiateVersion(ipcOffer, ipcOffer)
  })
}

export function createElectronIpcVersionAxes(
  core: HostNeutralBackendIdentity<string>['versions'],
  remoteOffer: IpcCompatibilityOffer
): IpcVersionAxes {
  const coreOffer = coreCompatibilityOffer(core)
  const ipcOffer = IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol
  return Object.freeze({
    ...negotiateCoreVersions(coreOffer, remoteOffer),
    ipcProtocol: negotiateVersion(ipcOffer, remoteOffer.ipcProtocol)
  })
}

function coreCompatibilityOffer(core: HostNeutralBackendIdentity<string>['versions']): IpcCompatibilityOffer {
  return {
    backendContract: versionRange(
      version('backend-contract', core.backendContract.selected.value),
      version('backend-contract', core.backendContract.selected.value)
    ),
    capabilitySchema: versionRange(
      version('capability-schema', core.capabilitySchema.selected.value),
      version('capability-schema', core.capabilitySchema.selected.value)
    ),
    eventSchema: versionRange(
      version('event-schema', core.eventSchema.selected.value),
      version('event-schema', core.eventSchema.selected.value)
    ),
    traceFormat: versionRange(
      version('trace-format', core.traceFormat.selected.value),
      version('trace-format', core.traceFormat.selected.value)
    ),
    ipcProtocol: IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol
  }
}

function rendererIdentity<Renderer extends string>(
  sender: TrustedIpcSender<string, Renderer>
): RendererIdentity<string, Renderer> {
  if (typeof sender.authenticatedWindowScope !== 'string' || sender.authenticatedWindowScope.trim().length === 0)
    throw contractError('ownership.denied', 'ipc', 'electron.ordered.window-scope')
  return Object.freeze({
    clientId: sender.authenticatedClientId,
    windowScope: sender.authenticatedWindowScope,
    sessionScope: sender.authenticatedSessionScope
  })
}

/** Only handles admitted by this exact route may be compensated. Concurrent
 * renderer operations share maps, but never share acquisition ownership. */
function isResourceAcquisitionCommand(command: string): boolean {
  return [
    'scan.start',
    'connection.connect',
    'security.watch.subscribe',
    'connection.write-readiness.subscribe',
    'connection.parameters.subscribe',
    'connection.events.subscribe',
    'gatt.discover',
    'gatt.acquire-write',
    'gatt.acquire-notifications',
    'gatt.ordered.open',
    'gatt.subscribe'
  ].includes(command)
}

function createOperationAcquisitions(): OperationAcquisitions {
  return {
    securityWatches: new Set(),
    readinessWatches: new Set(),
    parameterWatches: new Set(),
    scans: new Set(),
    connections: new Set(),
    connectionEventSubscriptions: new Set(),
    databases: new Set(),
    subscriptions: new Set(),
    orderedGroups: new Set(),
    acquiredWriters: new Set()
  }
}

function createManagedOperation(controller: AbortController): ManagedOperation {
  let complete = (): void => undefined
  const settled = new Promise<void>(resolve => {
    complete = resolve
  })
  return {
    controller,
    settled,
    complete
  }
}

function decodePreferredPhy(value: unknown): readonly ('le-1m' | 'le-2m' | 'le-coded')[] | undefined {
  if (value === undefined) return undefined
  if (!Array.isArray(value) || value.length === 0)
    throw contractError('argument.invalid', 'connection', 'electron.connect.preferred-phy')
  return Array.from(value, entry => {
    if (entry !== 'le-1m' && entry !== 'le-2m' && entry !== 'le-coded')
      throw contractError('argument.invalid', 'connection', 'electron.connect.preferred-phy')
    return entry
  })
}

function requiredString(payload: SerializableRecord, key: string): string {
  const value = payload[key]
  if (typeof value !== 'string' || value.length === 0) {
    throw contractError('protocol.malformed', 'ipc', `electron-main-router.${key}`)
  }
  return value
}

function terminalEvidenceReservationRequest(payload: SerializableRecord): TerminalEvidenceReservationRequest {
  const maximumBytes = payload.maximumBytes
  if (
    payload.version !== 1 ||
    payload.formatVersion !== 1 ||
    (payload.producer !== 'scan' && payload.producer !== 'subscription') ||
    typeof maximumBytes !== 'number' ||
    !Number.isSafeInteger(maximumBytes) ||
    maximumBytes < 1 ||
    !isSerializableRecordValue(payload.scope)
  ) {
    throw contractError('argument.invalid', 'restoration', 'electron.terminal-evidence.reserve')
  }
  const scope = payload.scope
  return Object.freeze({
    producer: payload.producer,
    maximumBytes,
    formatVersion: 1,
    scope: Object.freeze({
      managerId: requiredString(scope, 'managerId'),
      operationId: requiredString(scope, 'operationId'),
      incarnation: requiredString(scope, 'incarnation')
    })
  })
}

function terminalEvidenceCommitRequest(
  payload: SerializableRecord,
  binaryPayload: OwnedBytes | null
): { readonly reservationId: string; readonly digest: string; readonly canonicalBytes: Uint8Array } {
  if (payload.version !== 1 || binaryPayload === null)
    throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.commit')
  const digest = requiredString(payload, 'digest')
  if (!/^[0-9a-f]{64}$/.test(digest) || binaryPayload.byteLength === 0)
    throw contractError('argument.invalid', 'restoration', 'electron.terminal-evidence.commit')
  return Object.freeze({
    reservationId: requiredString(payload, 'reservationId'),
    digest,
    canonicalBytes: new Uint8Array(binaryPayload)
  })
}

function terminalEvidenceReference(payload: SerializableRecord, operation: string): string {
  if (payload.version !== 1) throw contractError('protocol.malformed', 'ipc', operation)
  return requiredString(payload, operation.endsWith('.release') ? 'reservationId' : 'ref')
}

function terminalEvidenceRecoveryList(payload: SerializableRecord, binaryPayload: OwnedBytes | null): void {
  if (binaryPayload !== null || payload.version !== 1)
    throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.recovery.list')
}

function terminalEvidenceRecoveryRead(
  payload: SerializableRecord,
  binaryPayload: OwnedBytes | null
): { readonly handle: string; readonly maximumBytes: number } {
  const maximumBytes = payload.maximumBytes
  if (
    binaryPayload !== null ||
    payload.version !== 1 ||
    typeof maximumBytes !== 'number' ||
    !Number.isSafeInteger(maximumBytes) ||
    maximumBytes < 1
  )
    throw contractError('argument.invalid', 'restoration', 'electron.terminal-evidence.recovery.read')
  return Object.freeze({ handle: requiredString(payload, 'handle'), maximumBytes })
}

function terminalEvidenceRecoveryAck(payload: SerializableRecord, binaryPayload: OwnedBytes | null): string {
  if (binaryPayload !== null || payload.version !== 1)
    throw contractError('protocol.malformed', 'ipc', 'electron.terminal-evidence.recovery.ack')
  return requiredString(payload, 'handle')
}

function isSerializableRecordValue(value: SerializableRecord[string] | undefined): value is SerializableRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function requiredDeliveryMode(
  value: SerializableRecord[string] | undefined
): 'prefer-notification' | 'prefer-indication' | 'require-notification' | 'require-indication' | undefined {
  if (value === undefined) return undefined
  if (
    value !== 'prefer-notification' &&
    value !== 'prefer-indication' &&
    value !== 'require-notification' &&
    value !== 'require-indication'
  ) {
    throw contractError('argument.invalid', 'gatt', 'electron-main-router.delivery-mode')
  }
  return value
}

function deliveryFromPayload(payload: SerializableRecord): SubscriptionOptions['delivery'] {
  const itemCapacity = payload.streamItemCapacity
  const byteCapacity = payload.streamByteCapacity
  const reservedControlCapacity = payload.streamReservedControlCapacity
  const overflowPolicy = payload.streamOverflowPolicy
  if (
    itemCapacity === undefined &&
    byteCapacity === undefined &&
    reservedControlCapacity === undefined &&
    overflowPolicy === undefined
  ) {
    return DEFAULT_DELIVERY
  }
  if (
    typeof itemCapacity !== 'number' ||
    typeof byteCapacity !== 'number' ||
    typeof reservedControlCapacity !== 'number' ||
    (overflowPolicy !== 'latest' &&
      overflowPolicy !== 'drop-oldest' &&
      overflowPolicy !== 'drop-newest' &&
      overflowPolicy !== 'error')
  ) {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.delivery')
  }
  try {
    return Object.freeze({
      itemCapacity: capacity(itemCapacity),
      byteCapacity: capacity(byteCapacity),
      reservedControlCapacity: capacity(reservedControlCapacity),
      overflowPolicy
    })
  } catch {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.delivery')
  }
}

/**
 * Admits the renderer's relative `budgetMs` against main's own monotonic clock,
 * read once when the request is received, and returns the payload with the
 * resulting main-clock `deadline`. The renderer's clock has a different time
 * origin, so an absolute renderer `deadline` is rejected rather than compared
 * with main's clock. Everything after receipt, including queueing, is charged
 * to the admitted budget.
 */
function admitRelativeBudget(payload: SerializableRecord, receiptClock: () => number): SerializableRecord {
  const { budgetMs, ...withoutBudget } = payload
  if (withoutBudget.deadline !== undefined && withoutBudget.deadline !== null) {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.budget')
  }
  if (budgetMs === undefined) return payload
  if (typeof budgetMs !== 'number' || !Number.isSafeInteger(budgetMs) || budgetMs < 0) {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.budget')
  }
  return { ...withoutBudget, deadline: receiptClock() + budgetMs }
}

function deadlineFromPayload(payload: SerializableRecord) {
  const value = payload.deadline
  if (value === null || value === undefined) {
    return null
  }
  if (typeof value !== 'number') {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.deadline')
  }
  return deadline(value)
}

function operationAdmissionFailure(
  controller: AbortController,
  payload: SerializableRecord,
  monotonicNow: () => number,
  command: string
): BackendContractError | null {
  if (controller.signal.aborted) {
    return contractError('operation.aborted', 'ipc', `electron-main-router.${command}`)
  }
  const operationDeadline = deadlineFromPayload(payload)
  if (operationDeadline !== null && operationDeadline <= monotonicNow()) {
    return contractError('operation.timed-out', 'ipc', `electron-main-router.${command}`)
  }
  return null
}

function isDestructiveCleanupCommand(command: string): boolean {
  return (
    command === 'security.watch.unsubscribe' ||
    command === 'connection.write-readiness.unsubscribe' ||
    command === 'scan.stop' ||
    command === 'connection.disconnect' ||
    command === 'connection.events.unsubscribe' ||
    command === 'gatt.unsubscribe' ||
    command === 'gatt.ordered.close' ||
    command === 'gatt.ordered.forget' ||
    command === 'gatt.ordered.retire' ||
    command === 'gatt.ordered.orphan-ack' ||
    command === 'gatt.ordered-recording.retire' ||
    command === 'gatt.ordered-recording.reclaim.cancel' ||
    command === 'gatt.ordered-recording.reclaim.release' ||
    command === 'gatt.ordered-recording.reclaim.retire' ||
    command === 'gatt.ordered-recording.reclaim.ack-retirement' ||
    command === 'gatt.ordered-recording.reclaim.forget-retirement' ||
    command === 'gatt.ordered-recording.reclaim.confirm-forgotten' ||
    command === 'gatt.acquired-write.close' ||
    command === 'gatt.database.release'
  )
}

/** Every ordered route requires the IPC-9 ordered failure-observation contract. */
function isOrderedRouteCommand(command: string): boolean {
  return command.startsWith('gatt.ordered.') || command.startsWith('gatt.ordered-recording.')
}

/**
 * Commands whose completed result is reported even when cancellation or the
 * deadline arrived after dispatch. Their effect cannot be rolled back: a
 * cleanup has released its resource, and a write has committed at the
 * peripheral. Reporting either as aborted/timed-out would invite the caller to
 * repeat an effect that already happened.
 */
function reportsCompletedEffect(command: string): boolean {
  return (
    isDestructiveCleanupCommand(command) ||
    command === 'gatt.ordered.open' ||
    command === 'gatt.ordered-recording.reserve' ||
    command === 'gatt.ordered-recording.start' ||
    command.startsWith('gatt.ordered-recording.reclaim.') ||
    command === 'gatt.ordered-recording.recovery' ||
    command.startsWith('gatt.ordered.marker.') ||
    command.startsWith('gatt.ordered-recording.marker.') ||
    command === 'gatt.ordered.next' ||
    command === 'connection.request-priority' ||
    command === 'gatt.write' ||
    command === 'gatt.acquired-write' ||
    command === 'gatt.write-when-ready' ||
    command === 'gatt.descriptor.write' ||
    command === 'security.pair' ||
    command === 'security.cancel-pairing' ||
    command === 'security.custom-ceremony' ||
    command === 'security.unpair'
  )
}

function operationOptions(payload: SerializableRecord, controller: AbortController) {
  return Object.freeze({ signal: controller.signal, deadline: deadlineFromPayload(payload) })
}

function optionalRediscoveryReason(
  payload: SerializableRecord
): Extract<GattDatabaseChangedEvent['reason'], 'service-changed' | 'manual-rediscovery'> | null {
  const value = payload.rediscoveryReason
  if (value === undefined) {
    return null
  }
  if (value !== 'service-changed' && value !== 'manual-rediscovery') {
    throw contractError('protocol.malformed', 'ipc', 'electron-main-router.rediscovery-reason')
  }
  return value
}

function requiredWriteMode(payload: SerializableRecord): 'with-response' | 'without-response' {
  const mode = requiredString(payload, 'mode')
  if (mode !== 'with-response' && mode !== 'without-response') {
    throw contractError('argument.invalid', 'ipc', 'electron-main-router.write-mode')
  }
  return mode
}

function requiredResource<Value>(resources: Map<string, Value>, handle: string, kind: string): Value {
  const resource = resources.get(handle)
  if (resource === undefined) {
    throw contractError('ownership.denied', 'ipc', `electron-main-router.${kind}-ownership`)
  }
  return resource
}

/**
 * Finding 211: the resource is gone because main already released it — a
 * source-terminal auto-removal (link loss ends the stream) or an earlier
 * explicit release. Releasing it again reports `released`: the end state
 * the caller asked for already holds. A handle main never issued is still
 * foreign and stays `ownership.denied`, so idempotence never blesses
 * guesses. Like `releaseDatabase`, which already answers `released` for an
 * unknown database.
 */
function releasedHandleTombstone(resources: RendererResources, handle: string): boolean {
  return resources.releasedHandles.has(handle)
}

function parameterIpcItem(value: {
  readonly kind: string
  readonly value?: {
    readonly connectionId: string
    readonly connectionGeneration: string
    readonly intervalUs: number
    readonly latency: number
    readonly supervisionTimeoutUs: number
    readonly observedAtMonotonicMs: number
    readonly ordinal: number
  }
  readonly reason?: string
  readonly error?: unknown
  readonly policy?: string
  readonly droppedItems?: number
  readonly droppedBytes?: number
  readonly replacedItems?: number
}): SerializableRecord {
  if (value.kind === 'value' && value.value !== undefined) {
    return Object.freeze({
      kind: 'value',
      value: Object.freeze({
        connectionId: String(value.value.connectionId),
        connectionGeneration: String(value.value.connectionGeneration),
        intervalUs: value.value.intervalUs,
        latency: value.value.latency,
        supervisionTimeoutUs: value.value.supervisionTimeoutUs,
        observedAtMonotonicMs: value.value.observedAtMonotonicMs,
        ordinal: value.value.ordinal
      })
    })
  }
  if (value.kind === 'overflow') {
    return Object.freeze({
      kind: 'overflow',
      policy: value.policy ?? 'drop-oldest',
      droppedItems: value.droppedItems ?? 0,
      droppedBytes: value.droppedBytes ?? 0,
      replacedItems: value.replacedItems ?? 0
    })
  }
  return Object.freeze({
    kind: 'terminal',
    reason: value.reason ?? 'source-failed',
    droppedItems: value.droppedItems ?? 0,
    droppedBytes: value.droppedBytes ?? 0,
    replacedItems: value.replacedItems ?? 0,
    error: value.error == null ? null : serializeNormalizedError(normalizedCleanupError(value.error))
  })
}

function readinessIpcItem(value: {
  readonly kind: string
  readonly value?: {
    readonly connectionId: string
    readonly connectionGeneration: string
    readonly ready: boolean
    readonly observedAtMonotonicMs: number
    readonly ordinal: number
  }
  readonly reason?: string
  readonly error?: unknown
  readonly policy?: string
  readonly droppedItems?: number
  readonly droppedBytes?: number
  readonly replacedItems?: number
}): SerializableRecord {
  if (value.kind === 'value' && value.value !== undefined) {
    return Object.freeze({
      kind: 'value',
      value: Object.freeze({
        connectionId: String(value.value.connectionId),
        connectionGeneration: String(value.value.connectionGeneration),
        ready: value.value.ready,
        observedAtMonotonicMs: value.value.observedAtMonotonicMs,
        ordinal: value.value.ordinal
      })
    })
  }
  if (value.kind === 'overflow') {
    return Object.freeze({
      kind: 'overflow',
      policy: value.policy ?? 'drop-oldest',
      droppedItems: value.droppedItems ?? 0,
      droppedBytes: value.droppedBytes ?? 0,
      replacedItems: value.replacedItems ?? 0
    })
  }
  return Object.freeze({
    kind: 'terminal',
    reason: value.reason ?? 'source-failed',
    droppedItems: value.droppedItems ?? 0,
    droppedBytes: value.droppedBytes ?? 0,
    replacedItems: value.replacedItems ?? 0,
    error: value.error == null ? null : serializeNormalizedError(normalizedCleanupError(value.error))
  })
}

function alreadyReleasedCleanup(): SerializableRecord {
  return Object.freeze({ state: 'released', failures: Object.freeze([]) })
}

function characteristicKey(path: {
  readonly serviceUuid: unknown
  readonly serviceOccurrence: unknown
  readonly characteristicUuid: unknown
  readonly characteristicOccurrence: unknown
}): string {
  return [path.serviceUuid, path.serviceOccurrence, path.characteristicUuid, path.characteristicOccurrence]
    .map(value => String(value))
    .join('|')
}

function characteristicProperties(characteristic: MainCharacteristicSnapshot): readonly string[] {
  const properties = characteristic.properties ?? {
    broadcast: false,
    read: false,
    writeWithResponse: false,
    writeWithoutResponse: false,
    authenticatedSignedWrites: false,
    notify: false,
    indicate: false,
    extendedProperties: false,
    reliableWrite: false,
    writableAuxiliaries: false
  }
  const entries: readonly (readonly [string, boolean | undefined])[] = [
    ['broadcast', properties.broadcast],
    ['read', properties.read],
    ['write', properties.writeWithResponse],
    ['write-with-response', properties.writeWithResponse],
    ['write-without-response', properties.writeWithoutResponse],
    ['authenticated-signed-writes', properties.authenticatedSignedWrites],
    ['notify', properties.notify],
    ['indicate', properties.indicate],
    ['extended-properties', properties.extendedProperties],
    ['reliable-write', properties.reliableWrite],
    ['writable-auxiliaries', properties.writableAuxiliaries]
  ]
  return Object.freeze(entries.flatMap(([name, supported]) => (supported ? [name] : [])))
}

function discoveryDescriptor(capabilities: ReturnType<MainManager['capabilities']>): {
  readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid'
} {
  const supports = (id: string): boolean => {
    const descriptor = capabilities.find(candidate => String(candidate.id) === id)
    return descriptor?.state === 'supported' || descriptor?.state === 'limited'
  }
  const continuous = supports('discovery:continuous-scan')
  const chooser = supports('discovery:system-chooser')
  if (continuous && chooser) return Object.freeze({ kind: 'hybrid' })
  if (chooser) return Object.freeze({ kind: 'system-chooser' })
  return Object.freeze({ kind: 'continuous-scan' })
}

function serializeAccessRequirements(access: GattAccessRequirements): SerializableRecord {
  return Object.freeze({ read: access.read, write: access.write })
}

function serializeCharacteristicProperties(properties: CharacteristicProperties): SerializableRecord {
  return Object.freeze({
    broadcast: properties.broadcast,
    read: properties.read,
    writeWithResponse: properties.writeWithResponse,
    writeWithoutResponse: properties.writeWithoutResponse,
    authenticatedSignedWrites: properties.authenticatedSignedWrites,
    notify: properties.notify,
    indicate: properties.indicate,
    extendedProperties: properties.extendedProperties,
    reliableWrite: properties.reliableWrite,
    writableAuxiliaries: properties.writableAuxiliaries,
    availability: Object.freeze({ ...properties.availability })
  })
}

function serializeDescriptorProperties(properties: GattDescriptorProperties): SerializableRecord {
  return Object.freeze({
    read: properties.read,
    write: properties.write,
    availability: Object.freeze({ ...properties.availability }),
    access: serializeAccessRequirements(properties.access)
  })
}

function serializeServiceRestriction(restriction: GattServiceRestriction): SerializableRecord {
  return Object.freeze({
    state: restriction.state,
    reason: restriction.reason,
    gattStatus: restriction.gattStatus,
    attError: restriction.attError
  })
}

function cleanupRecord(cleanup: CleanupRecord): SerializableRecord {
  return Object.freeze({
    state: cleanup.state,
    failures: Object.freeze(
      cleanup.failures.map(failure =>
        Object.freeze({
          resourceKind: failure.resourceKind,
          error: serializeNormalizedError(failure.error)
        })
      )
    )
  })
}

function serializeWriteReceipt(
  receipt: {
    readonly terminal: {
      readonly correlation: string
      readonly outcome: string
      readonly cause: string | null
    }
    readonly commitState: 'confirmed' | 'unknown'
  },
  mode: 'with-response' | 'without-response',
  bytesSubmitted: number
): SerializableRecord {
  return Object.freeze({
    terminal: Object.freeze({
      correlation: receipt.terminal.correlation,
      outcome: receipt.terminal.outcome === 'succeeded' ? 'succeeded' : 'failed',
      cause: receipt.terminal.cause
    }),
    mode,
    commitState: receipt.commitState,
    bytesSubmitted
  })
}

function normalizedCleanupError(error: unknown) {
  if (error instanceof BackendContractError) {
    return error.normalized
  }
  return contractError('platform.failure', 'cleanup', 'electron-main-router.cleanup').normalized
}

function pruneExpiredCorrelationRecords(records: Map<string, number>, now: number): void {
  for (const [correlation, expiresAt] of records) {
    if (expiresAt <= now) {
      records.delete(correlation)
    }
  }
}

function trimCorrelationRecords(records: Map<string, number>, maximumRecords: number): void {
  while (records.size > maximumRecords) {
    const oldest = records.keys().next().value
    if (oldest === undefined) {
      return
    }
    records.delete(oldest)
  }
}
