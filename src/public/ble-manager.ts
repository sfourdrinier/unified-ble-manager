// src/public/ble-manager.ts — non-generic application façade (PR1 skeleton)

import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { ScanOptions as InternalScanOptions } from '../backend-contract/advertisement'
import type { ConnectionLifecycleCause, ConnectionLifecycleEvent } from '../backend-contract/connection-lifecycle'
import { contractError, type CleanupRecord } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import { capacity, canonicalUuid, createAttachmentBoundPeerId } from '../backend-contract/primitives'
import type { PeerId } from '../backend-contract/primitives'
import type { BleManager as InternalBleManager } from '../manager/ble-manager'
import type { BleManagerOptions } from '../manager/ble-manager'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import type { BoundedAsyncStreamIterator, StreamItem, StreamTerminalNotice } from '../backend-contract/streams'
import { CoreBoundedStream } from '../core/bounded-stream'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import { resolveStreamPolicy } from './stream-presets'
import type { StreamBudget, StreamPolicy } from './stream-presets'
import type { IpcAdvertisement } from '../ipc/manager'
import {
  BleCleanupError,
  collectCleanupPhases,
  rehydratePublicError,
  rehydratePublicPromise,
  runWithCleanup
} from './error-bridge'
import { assertDirectConnectionCapability, PublicBleCapabilities } from './capabilities'
import type { BleCapabilities } from './capabilities'
import type {
  BleAdapter,
  BleAdapterState,
  AdapterReadinessOptions,
  AdapterWatchOptions,
  BleAdapterStateWatch
} from './ble-adapter'
import type { BleDiagnostics } from './diagnostics'
import { snapshotResourceCounters } from './diagnostics'
import { isAuthorizationBlocking, type AdapterStateSnapshot } from '../backend-contract/identity'
import { createPublicGattDatabase } from './gatt'
import type { GattDatabase, GattValueEvent } from './gatt'
import {
  normalizeScanObservation,
  normalizeScanQuery,
  observationMatchesScanQuery,
  type NormalizedScanObservation,
  type ScanQuery
} from './scan-query'
import { createScanState } from './scan-state'
import type { BlePeerDirectory, BlePeerState, PeerSource } from './peer-directory'
import { createPublicPeerDirectory } from './peer-directory'
import { encodePeerReference, isPeerReference, snapshotPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'
import type { ResourceCounters } from '../backend-contract/backend'
import type { ScanPlan } from '../backend-contract/scan-planning'
export type { ScanPlan } from '../backend-contract/scan-planning'
import { createPublicSecurity } from './security'
import type { BleSecurity } from './security'
import type { Limitation } from '../backend-contract/capabilities'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type ConnectionPriority,
  type ConnectionWriteReadinessObservation,
  type ConnectionWriteReadinessWatch
} from '../backend-contract/connection-controls'
import { MAX_PUBLIC_SCAN_STATE_BYTES, MAX_PUBLIC_SCAN_STATE_ENTRIES } from './scan-state-budget'

export type { ConnectionPriority } from '../backend-contract/connection-controls'

type PublicInternalManager<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
> = InternalBleManager<Attachment, Identity>

export type GattSubscriptionValue = GattValueEvent
export type ConnectionIntent = 'direct' | 'when-available'
export interface ConnectOptions extends OperationOptions {
  readonly intent?: ConnectionIntent
  readonly transport?: 'le' | 'auto'
  readonly preferredPhy?: readonly BlePhy[]
}
export interface BleConnectionEvent {
  readonly kind: 'connection-lifecycle'
  readonly previous: ConnectionLifecycleEvent<string>['previous']
  readonly current: ConnectionLifecycleEvent<string>['current']
  readonly cause: ConnectionLifecycleCause
  readonly connectionGeneration: string
  readonly sequence: number
}

export type BleControlObservationState = 'measured' | 'unavailable' | 'unsupported'
export type BleObservationSource = 'backend' | 'platform' | 'core' | 'unknown'

export interface BleControlObservationMetadata {
  readonly connectionGeneration: string
  readonly observedAtMonotonicMs: number
  readonly source: BleObservationSource
  readonly authority: string
  readonly limitations: readonly Limitation[]
}

export interface RssiObservation extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly rssi: number | null
}

export interface MtuObservation extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly attMtu: number | null
  readonly payloadBytes: number | null
  readonly platformPduBytes: number | null
}

export type MtuNegotiationState = 'accepted' | 'rejected' | 'unavailable' | 'unsupported'

export interface MtuNegotiation extends BleControlObservationMetadata {
  readonly state: MtuNegotiationState
  readonly requestedMtu: number
  readonly observation: MtuObservation | null
}

export type BlePhy = 'le-1m' | 'le-2m' | 'le-coded'
export type PhyPreference = Readonly<{
  readonly tx?: BlePhy
  readonly rx?: BlePhy
}>
export type SubrateMode = 'default' | 'low-latency' | 'low-power'
export type WriteMode = 'with-response' | 'without-response'

export interface MaximumWriteLengthObservation extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly mode: WriteMode
  readonly maximumWriteLength: number | null
}

export interface ConnectionPriorityResult extends BleControlObservationMetadata {
  readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'
  readonly requested: ConnectionPriority
}

export interface PhyObservation extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly tx: BlePhy | null
  readonly rx: BlePhy | null
}

export interface PhyUpdateResult extends BleControlObservationMetadata {
  readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'
  readonly requested: PhyPreference
  readonly observation: PhyObservation | null
}

export interface ConnectionParametersObservation extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly intervalMs: number | null
  readonly peripheralLatency: number | null
  readonly supervisionTimeoutMs: number | null
  readonly subrateFactor: number | null
  readonly connectionEventLengthMs: number | null
}

export interface SubrateResult extends BleControlObservationMetadata {
  readonly state: 'accepted' | 'rejected' | 'unavailable' | 'unsupported'
  readonly requested: SubrateMode
  readonly observation: ConnectionParametersObservation | null
}

export interface WriteReadinessEvent extends BleControlObservationMetadata {
  readonly state: BleControlObservationState
  readonly mode: 'without-response'
  readonly ready: boolean | null
}

export interface BleConnectionControls {
  readRssi(options?: OperationOptions): Promise<RssiObservation>
  effectiveMtu(): Promise<MtuObservation>
  requestMtu(mtu: number, options?: OperationOptions): Promise<MtuNegotiation>
  maximumWriteLength(mode: WriteMode): Promise<MaximumWriteLengthObservation>
  requestPriority(priority: ConnectionPriority, options?: OperationOptions): Promise<ConnectionPriorityResult>
  readPhy(options?: OperationOptions): Promise<PhyObservation>
  requestPhy(preference: PhyPreference, options?: OperationOptions): Promise<PhyUpdateResult>
  parameters(): Promise<ConnectionParametersObservation>
  parameterEvents(): AsyncIterable<ConnectionParametersObservation>
  requestSubrate(mode: SubrateMode, options?: OperationOptions): Promise<SubrateResult>
  writeReadiness(mode: 'without-response'): AsyncIterable<WriteReadinessEvent>
}

export interface RediscoverGattOptions extends OperationOptions {
  readonly reason: 'service-changed' | 'manual'
}
export type {
  GattDatabase,
  GattDatabaseSnapshot,
  GattService,
  GattCharacteristic,
  GattDescriptor,
  GattSubscription,
  GattValueEvent,
  GattValueStream,
  GattDatabaseChangedEvent,
  GattWriteReceipt,
  GattLongWriteReceipt,
  GattCharacteristicProperties,
  GattAccessRequirements,
  GattServiceReference,
  GattWriteOptions,
  LongWriteOptions,
  DescriptorWriteOptions,
  GattSubscribeOptions,
  OccurrenceSelector,
  GattPathSelector,
  UuidInput
} from './gatt'
export type {
  ManufacturerDataPattern,
  NormalizedManufacturerDataPattern,
  NormalizedScanClause,
  NormalizedScanObservation,
  NormalizedScanQuery,
  NormalizedServiceDataPattern,
  ScanClause,
  ScanQuery,
  ServiceDataPattern
} from './scan-query'
export type { BlePeerDirectory, BlePeerState, KnownPeerQuery, PeerSource } from './peer-directory'
export type { PeerReference, PeerReferenceScope } from './peer-reference'
export type {
  BleSecurity,
  PairCancelResult,
  PairingAgent,
  PairingChallenge,
  PairingResponse,
  PairOptions,
  RequiredSecurityOptions,
  PairResult,
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityAuthenticationState,
  SecurityBondState,
  SecurityEncryptionState,
  SecureConnectionsState,
  SecurityPeer,
  UnpairResult,
  SecurityRequirement
} from './security'

// Public peer — opaque backend-scoped identifier, no generic.
export interface BlePeer {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
  readonly reference: PeerReference | null
  readonly sources: readonly PeerSource[]
  readonly lastAdvertisement: NormalizedScanObservation | null
  readonly state?: BlePeerState
}

type BlePeerInput = Pick<BlePeer, 'id' | 'name' | 'rssi'> &
  Partial<Pick<BlePeer, 'reference' | 'sources' | 'lastAdvertisement' | 'state'>>

export function snapshotBlePeer(peer: BlePeerInput): BlePeer {
  return Object.freeze({
    id: peer.id,
    name: peer.name,
    rssi: peer.rssi,
    reference:
      peer.reference === undefined || peer.reference === null
        ? null
        : snapshotPeerReference(peer.reference, 'peer.snapshot'),
    sources: Object.freeze([...(peer.sources ?? [])]),
    lastAdvertisement:
      peer.lastAdvertisement === undefined || peer.lastAdvertisement === null
        ? null
        : normalizeScanObservation(peer.lastAdvertisement),
    ...(peer.state === undefined ? {} : { state: Object.freeze({ ...peer.state }) })
  })
}

// Public connection — generation-bound lease, no generic.
export interface BleConnection {
  readonly peer: BlePeer
  readonly connectionGeneration: string
  readonly lifecycleEvents: AsyncIterable<BleConnectionEvent>
  readonly controls: BleConnectionControls
  readonly discover: (options?: OperationOptions) => Promise<GattDatabase>
  readonly rediscoverGatt: (options: RediscoverGattOptions) => Promise<GattDatabase>
  readonly disconnect: () => Promise<CleanupRecord>
  readonly release: () => Promise<CleanupRecord>
}

// Public scan session — bounded stream, no generic.
// Union embraces both native AdvertisementObservation and Tauri IpcAdvertisement
// until PR4 scan semantics unify; covariance lets each backend stream satisfy the union without casts.
export interface PublicScanObservation extends NormalizedScanObservation {
  readonly peer: BlePeer
  readonly observedAtMonotonicMs: number | null
}

export type DiscoveryEvent =
  | {
      readonly kind: 'observed'
      readonly peer: BlePeer
    }
  | {
      readonly kind: 'lost'
      readonly peer: BlePeer
      readonly lastObservedAt: number
      readonly derivedAt: number
      readonly reason: 'observation-timeout'
    }

export type AndroidScanMode = 'low-power' | 'balanced' | 'low-latency' | 'opportunistic'
export type AndroidScanCallbackType = 'all-matches' | 'first-match' | 'match-lost'
export type AndroidScanPhy = 'all-supported' | '1m' | 'coded'
export interface AndroidScanPlatformOptions {
  readonly kind: 'android'
  readonly mode?: AndroidScanMode
  readonly callbackType?: AndroidScanCallbackType
  readonly reportDelayMs?: number
  readonly legacy?: boolean
  readonly phy?: AndroidScanPhy
}
export type ScanPlatformOptions =
  | AndroidScanPlatformOptions
  | { readonly kind: 'corebluetooth' }
  | { readonly kind: 'winrt' }
  | { readonly kind: 'web' }
  | { readonly kind: 'electron' }
  | { readonly kind: 'tauri' }

export interface ScanSession {
  readonly plan: ScanPlan | null
  readonly stop: () => Promise<CleanupRecord>
  readonly observations: BoundedAsyncStream<PublicScanObservation>
  readonly events?: AsyncIterable<DiscoveryEvent>
  readonly state: AsyncIterable<ScanStateEvent>
}

export interface PublicScanFingerprintAccounting {
  readonly fingerprintCount: number
  readonly fingerprintBytes: number
  readonly summedEntryBytes: number
}

const publicScanFingerprintInspectors = new WeakMap<ScanSession, () => PublicScanFingerprintAccounting>()

export function inspectPublicScanFingerprintAccountingForTests(session: ScanSession): PublicScanFingerprintAccounting {
  const inspect = publicScanFingerprintInspectors.get(session)
  if (inspect === undefined) {
    throw contractError('argument.invalid', 'scan', 'public-scan.fingerprint-inspect')
  }
  return inspect()
}

export type ScanStateEvent = {
  readonly state: 'starting' | 'active' | 'stopping' | 'stopped' | 'failed'
  readonly reason?: string
}

// Non-generic public manager. Lifecycle/ownership/generations stay in core.
export interface BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly security: BleSecurity
  readonly discovery: BleDiscoveryInfo
  readonly destroy: () => Promise<CleanupRecord>
  scan(options?: ScanOptions): Promise<ScanSession>
  find(options?: FindOptions): Promise<BlePeer>
  choose(options?: ChooseOptions): Promise<BlePeer>
  connect(peer: BlePeer | string | PeerReference, options?: ConnectOptions): Promise<BleConnection>
  withConnection<T>(
    peer: BlePeer | string | PeerReference,
    options: ConnectOptions,
    action: (connection: BleConnection) => Promise<T>
  ): Promise<T>
  withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T>
  withDiscoveredConnection<T>(
    peer: BlePeer | string | PeerReference,
    options: ConnectOptions,
    action: (scope: { readonly connection: BleConnection; readonly gatt: GattDatabase }) => Promise<T>
  ): Promise<T>
}

export { PublicBleManager as BleManagerImpl }

export interface ScanOptions extends OperationOptions {
  readonly query?: ScanQuery
  readonly duplicates?: 'coalesced' | 'all'
  readonly delivery?: StreamPolicy
  readonly observation?: {
    readonly reportLostAfterMs?: number
    readonly includeRawAdvertisement?: boolean
  }
  readonly platform?: ScanPlatformOptions
}

export interface FindOptions extends OperationOptions {
  /** Defaults to 10 seconds when omitted. */
  readonly query?: ScanQuery
  readonly select?: 'first' | ((peer: BlePeer) => boolean)
}

export interface ChooseOptions extends OperationOptions {
  readonly filters?: readonly ChooseFilter[]
  readonly optionalServices?: readonly (string | number)[]
  readonly acceptAllDevices?: boolean
}

export interface ChooseFilter {
  readonly serviceUuids?: readonly (string | number)[]
  readonly manufacturerData?: readonly {
    readonly companyIdentifier: number
    readonly dataPrefix?: Readonly<Uint8Array>
  }[]
  readonly localNamePrefix?: string
}

export interface BleDiscoveryInfo {
  readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid'
}

export interface PublicBleManagerHostOptions<Attachment extends string = string> {
  readonly discoveryKind?: BleDiscoveryInfo['kind']
  readonly choose?: (options: ChooseOptions) => Promise<BlePeer>
  readonly peers?: BlePeerDirectory
  /** Explicit test seam for lightweight manager doubles without backend identity. */
  readonly peerId?: (value: string) => PeerId<Attachment>
}

interface PublicScanDeadlineHandle {
  cancel(): void
}

type InternalScanScheduler = (deadline: number, action: () => void) => PublicScanDeadlineHandle

function scheduleInternalScanDeadline<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>,
  deadline: number,
  action: () => void
): PublicScanDeadlineHandle {
  const scheduler: InternalScanScheduler = (deadlineAt, callback) => internal.scheduleDeadline(deadlineAt, callback)
  return scheduler(deadline, action)
}

interface PublicScanPresence {
  readonly observation: PublicScanObservation
  lastSeenAtMonotonicMs: number
  timer: PublicScanDeadlineHandle | null
  readonly bytes: number
}

interface PublicScanFingerprint {
  readonly value: string
  readonly bytes: number
}

type PublicScanEventTerminalReason = 'closed' | 'source-failed' | 'overflow' | 'owner-released'

class PublicScanEventBroadcast implements AsyncIterable<DiscoveryEvent> {
  private readonly subscribers = new Set<CoreBoundedStream<DiscoveryEvent>>()
  private terminalReason: PublicScanEventTerminalReason | null = null

  constructor(
    private readonly startPump: () => void,
    private readonly delivery: StreamBudget
  ) {}

  [Symbol.asyncIterator](): AsyncIterableIterator<DiscoveryEvent> {
    const stream = new CoreBoundedStream<DiscoveryEvent>(this.delivery, this.delivery.overflowPolicy)
    if (this.terminalReason === null) {
      this.subscribers.add(stream)
      this.startPump()
    } else {
      stream.closeWithReason(this.terminalReason)
    }
    const iterator = stream[Symbol.asyncIterator]()
    return {
      next: async () => {
        const item = await iterator.next()
        if (item.done) return { done: true, value: undefined }
        if (item.value.kind === 'value') return { done: false, value: item.value.value }
        if (item.value.kind === 'overflow') {
          throw rehydratePublicError(contractError('stream.overflow', 'scan', 'public-scan.events'))
        }
        if (item.value.reason === 'overflow') {
          throw rehydratePublicError(contractError('stream.overflow', 'scan', 'public-scan.events'))
        }
        return { done: true, value: undefined }
      },
      return: async () => {
        this.subscribers.delete(stream)
        await iterator.return()
        return { done: true, value: undefined }
      },
      [Symbol.asyncIterator]() {
        return this
      }
    }
  }

  emit(event: DiscoveryEvent, byteLength: number): boolean {
    let terminated = false
    for (const subscriber of [...this.subscribers]) {
      const result = subscriber.emit(event, byteLength)
      if (result.terminated) {
        terminated = true
        this.subscribers.delete(subscriber)
      }
    }
    return terminated
  }

  close(reason: PublicScanEventTerminalReason): void {
    if (this.terminalReason !== null) return
    this.terminalReason = reason
    for (const subscriber of this.subscribers) {
      subscriber.closeWithReason(reason)
      this.subscribers.delete(subscriber)
    }
  }
}

class PublicScanSessionController<Attachment extends string> {
  readonly observations: BoundedAsyncStream<PublicScanObservation>
  readonly events: AsyncIterable<DiscoveryEvent>
  private readonly observationStream: CoreBoundedStream<PublicScanObservation>
  private readonly eventBroadcast: PublicScanEventBroadcast
  private readonly presence = new Map<string, PublicScanPresence>()
  private presenceBytes = 0
  private readonly lastObservationFingerprints = new Map<string, PublicScanFingerprint>()
  private fingerprintBytes = 0
  private sourceIterator: BoundedAsyncStreamIterator<AdvertisementObservation<Attachment> | IpcAdvertisement> | null =
    null
  private pumpStarted = false
  private closed = false

  constructor(
    private readonly source: BoundedAsyncStream<AdvertisementObservation<Attachment> | IpcAdvertisement>,
    private readonly query: ReturnType<typeof normalizeScanQuery>,
    private readonly duplicates: 'coalesced' | 'all',
    delivery: StreamBudget,
    private readonly now: () => number,
    private readonly scheduleDeadline: InternalScanScheduler,
    private readonly reportLostAfterMs: number | undefined,
    private readonly requestStop: (reason: PublicScanEventTerminalReason) => void
  ) {
    this.observationStream = new CoreBoundedStream(delivery, delivery.overflowPolicy)
    this.observations = {
      limits: this.observationStream.limits,
      overflowPolicy: this.observationStream.overflowPolicy,
      [Symbol.asyncIterator]: () => {
        this.start()
        return this.observationStream[Symbol.asyncIterator]()
      },
      close: () => this.close('closed')
    }
    this.eventBroadcast = new PublicScanEventBroadcast(() => this.start(), delivery)
    this.events = this.eventBroadcast
  }

  async close(reason: PublicScanEventTerminalReason = 'owner-released'): Promise<CleanupRecord> {
    return this.closeView(reason)
  }

  async closeView(reason: PublicScanEventTerminalReason = 'owner-released'): Promise<CleanupRecord> {
    this.closed = true
    this.cancelPresenceTimers()
    this.observationStream.closeWithReason(reason)
    this.eventBroadcast.close(reason)
    if (this.sourceIterator === null) return { state: 'released', failures: [] }
    await this.sourceIterator.return()
    this.sourceIterator = null
    return { state: 'released', failures: [] }
  }

  private terminateFromOverflow(): void {
    if (this.closed) {
      this.requestStop('overflow')
      return
    }
    this.closed = true
    this.cancelPresenceTimers()
    this.observationStream.closeWithReason('overflow')
    this.eventBroadcast.close('overflow')
    this.requestStop('overflow')
  }

  private start(): void {
    if (this.pumpStarted || this.closed) return
    this.pumpStarted = true
    const iterator = this.source[Symbol.asyncIterator]()
    this.sourceIterator = iterator
    this.pump(iterator).catch(() => undefined)
  }

  private async pump(
    iterator: BoundedAsyncStreamIterator<AdvertisementObservation<Attachment> | IpcAdvertisement>
  ): Promise<void> {
    try {
      while (!this.closed) {
        const item = await iterator.next()
        if (item.done) {
          await this.finish('closed')
          return
        }
        if (item.value.kind === 'overflow') {
          this.observationStream.observeSourceOverflow(item.value)
          this.eventBroadcast.close('overflow')
          continue
        }
        if (item.value.kind === 'terminal') {
          await this.finish(item.value.reason)
          return
        }
        this.accept(item.value.value)
      }
    } catch {
      await this.finish('source-failed')
    }
  }

  private accept(raw: AdvertisementObservation<Attachment> | IpcAdvertisement): void {
    const observation = projectPublicScanObservation(raw)
    if (!observationMatchesScanQuery(this.query, observation)) return

    this.observePresence(observation)
    if (this.duplicates === 'coalesced') {
      const fingerprint = publicObservationFingerprint(observation)
      const previous = this.lastObservationFingerprints.get(observation.peer.id)
      if (previous?.value === fingerprint) return
      this.rememberObservationFingerprint(observation.peer.id, fingerprint)
    }
    const observationPush = this.observationStream.emit(observation, estimatePublicScanObservationBytes(observation))
    const eventTerminated = this.eventBroadcast.emit(
      Object.freeze({ kind: 'observed', peer: observation.peer }),
      estimatePublicDiscoveryEventBytes({ kind: 'observed', peer: observation.peer })
    )
    if (observationPush.terminated || eventTerminated) this.terminateFromOverflow()
  }

  private observePresence(observation: PublicScanObservation): void {
    if (this.reportLostAfterMs === undefined) return
    const observedAt = observation.observedAtMonotonicMs ?? this.now()
    const current = this.presence.get(observation.peer.id)
    if (current !== undefined && observedAt < current.lastSeenAtMonotonicMs) {
      return
    }
    if (current !== undefined) {
      current.timer?.cancel()
      this.presence.delete(observation.peer.id)
      this.presenceBytes -= current.bytes
    }
    const presence: PublicScanPresence = {
      observation,
      lastSeenAtMonotonicMs: observedAt,
      timer: null,
      bytes: estimatePublicScanObservationBytes(observation)
    }
    this.presence.set(observation.peer.id, presence)
    this.presenceBytes += presence.bytes
    this.evictPresenceState()
    presence.timer = this.scheduleDeadline(observedAt + this.reportLostAfterMs, () => {
      this.reportLost(observation.peer.id, observedAt)
    })
  }

  private reportLost(peerId: string, expectedLastSeenAtMonotonicMs: number): void {
    if (this.closed || this.reportLostAfterMs === undefined) return
    const current = this.presence.get(peerId)
    if (current === undefined || current.lastSeenAtMonotonicMs !== expectedLastSeenAtMonotonicMs) return
    const dueAt = expectedLastSeenAtMonotonicMs + this.reportLostAfterMs
    const now = this.now()
    if (now < dueAt) {
      current.timer = this.scheduleDeadline(dueAt, () => this.reportLost(peerId, expectedLastSeenAtMonotonicMs))
      return
    }
    this.presence.delete(peerId)
    this.presenceBytes -= current.bytes
    this.forgetObservationFingerprint(peerId)
    current.timer = null
    this.eventBroadcast.emit(
      Object.freeze({
        kind: 'lost',
        peer: current.observation.peer,
        lastObservedAt: expectedLastSeenAtMonotonicMs,
        derivedAt: now,
        reason: 'observation-timeout'
      }),
      estimatePublicDiscoveryEventBytes({
        kind: 'lost',
        peer: current.observation.peer,
        lastObservedAt: expectedLastSeenAtMonotonicMs,
        derivedAt: now,
        reason: 'observation-timeout'
      })
    )
  }

  private cancelPresenceTimers(): void {
    for (const current of this.presence.values()) {
      current.timer?.cancel()
      current.timer = null
    }
    this.presence.clear()
    this.presenceBytes = 0
    this.lastObservationFingerprints.clear()
    this.fingerprintBytes = 0
    this.assertFingerprintAccounting()
  }

  fingerprintAccounting(): PublicScanFingerprintAccounting {
    let summedEntryBytes = 0
    for (const entry of this.lastObservationFingerprints.values()) {
      summedEntryBytes += entry.bytes
    }
    return {
      fingerprintCount: this.lastObservationFingerprints.size,
      fingerprintBytes: this.fingerprintBytes,
      summedEntryBytes
    }
  }

  private forgetObservationFingerprint(peerId: string): void {
    const existing = this.lastObservationFingerprints.get(peerId)
    if (existing === undefined) return
    this.lastObservationFingerprints.delete(peerId)
    this.fingerprintBytes -= existing.bytes
    this.assertFingerprintAccounting()
  }

  private rememberObservationFingerprint(peerId: string, value: string): void {
    this.forgetObservationFingerprint(peerId)
    const fingerprint = { value, bytes: value.length * 2 }
    this.lastObservationFingerprints.set(peerId, fingerprint)
    this.fingerprintBytes += fingerprint.bytes
    this.assertFingerprintAccounting()
    while (
      this.lastObservationFingerprints.size > MAX_PUBLIC_SCAN_STATE_ENTRIES ||
      this.fingerprintBytes > MAX_PUBLIC_SCAN_STATE_BYTES
    ) {
      const oldest = this.lastObservationFingerprints.entries().next().value
      if (oldest === undefined) return
      const [oldestPeerId] = oldest
      this.forgetObservationFingerprint(oldestPeerId)
    }
  }

  private evictPresenceState(): void {
    while (this.presence.size > MAX_PUBLIC_SCAN_STATE_ENTRIES || this.presenceBytes > MAX_PUBLIC_SCAN_STATE_BYTES) {
      const oldest = this.presence.entries().next().value
      if (oldest === undefined) return
      const [peerId, presence] = oldest
      presence.timer?.cancel()
      this.presence.delete(peerId)
      this.presenceBytes -= presence.bytes
      this.forgetObservationFingerprint(peerId)
    }
  }

  private assertFingerprintAccounting(): void {
    const accounting = this.fingerprintAccounting()
    if (accounting.fingerprintBytes !== accounting.summedEntryBytes || accounting.fingerprintBytes < 0) {
      throw contractError('protocol.violation', 'scan', 'public-scan.fingerprint-accounting')
    }
  }

  private async finish(reason: StreamTerminalNotice['reason']): Promise<void> {
    if (this.closed) return
    this.closed = true
    this.cancelPresenceTimers()
    this.observationStream.finishWithReason(reason)
    this.eventBroadcast.close(
      reason === 'source-failed'
        ? 'source-failed'
        : reason === 'overflow'
          ? 'overflow'
          : reason === 'owner-released'
            ? 'owner-released'
            : 'closed'
    )
  }
}

type InternalPublicConnection<Attachment extends string, Identity extends BackendIdentity<Attachment>> = Awaited<
  ReturnType<PublicInternalManager<Attachment, Identity>['connect']>
>

interface OptionalInternalControlConnection<Attachment extends string> {
  readonly effectiveMtu?: () => Promise<{
    readonly connectionId: string
    readonly connectionGeneration: string
    readonly attMtu: number | null
    readonly payloadBytes: number | null
    readonly platformPduBytes: number | null
    readonly observedAtMonotonicMs?: number
  }>
  readonly writeWithoutResponseReadiness?: () => Promise<ConnectionWriteReadinessWatch<Attachment>>
}

type PublicControlConnection<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
> = InternalPublicConnection<Attachment, Identity> & OptionalInternalControlConnection<Attachment>

function controlMetadata<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  generation: string,
  now: number,
  descriptor: ReturnType<PublicInternalManager<Attachment, Identity>['capability']>,
  authority: string
): BleControlObservationMetadata {
  return Object.freeze({
    connectionGeneration: generation,
    observedAtMonotonicMs: now,
    source: 'backend',
    authority,
    limitations: Object.freeze([...(descriptor?.limitations ?? [])])
  })
}

interface PublicConnectionIdentity {
  readonly connectionId: string
  readonly connectionGeneration: string
}

function assertPublicConnectionIdentity(
  expected: PublicConnectionIdentity,
  actual: PublicConnectionIdentity,
  operation: string
): void {
  if (
    typeof actual.connectionId !== 'string' ||
    typeof actual.connectionGeneration !== 'string' ||
    actual.connectionId !== expected.connectionId ||
    actual.connectionGeneration !== expected.connectionGeneration
  ) {
    throw contractError('protocol.violation', 'connection', operation)
  }
}

function requireControlCapability<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: Pick<PublicInternalManager<Attachment, Identity>, 'capability'>,
  id: `${string}:${string}`,
  operation: string
) {
  const descriptor = internal.capability(id)
  if (descriptor === null || descriptor.state === 'unsupported') {
    throw contractError('capability.unsupported', 'connection', operation)
  }
  if (descriptor.state === 'unavailable') {
    throw contractError('capability.unavailable', 'connection', operation)
  }
  return descriptor
}

async function runPublicControl<Value>(action: () => Promise<Value>): Promise<Value> {
  try {
    return await action()
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function unsupportedControlStream<Value>(
  operation: string,
  code: 'capability.unsupported' | 'capability.unavailable' = 'capability.unsupported'
): AsyncIterable<Value> {
  return new UnsupportedControlStream(operation, code)
}

function publicWriteReadinessStream<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  connection: PublicControlConnection<Attachment, Identity>,
  generation: string,
  descriptor: ReturnType<PublicInternalManager<Attachment, Identity>['capability']>
): AsyncIterable<WriteReadinessEvent> {
  return {
    [Symbol.asyncIterator](): AsyncIterator<WriteReadinessEvent> {
      let watch: ConnectionWriteReadinessWatch<Attachment> | null = null
      let iterator: BoundedAsyncStreamIterator<ConnectionWriteReadinessObservation<Attachment>> | null = null
      let closed = false
      let iteratorDone = false
      let teardownAttempted = false

      const open = async (): Promise<void> => {
        if (watch !== null) return
        const observe = connection.writeWithoutResponseReadiness
        if (observe === undefined) {
          throw contractError('capability.unsupported', 'connection', 'public-connection.controls.write-readiness')
        }
        watch = await observe()
        iterator = watch.events[Symbol.asyncIterator]()
      }

      const close = async (): Promise<void> => {
        if (teardownAttempted) return
        teardownAttempted = true
        if (watch === null || iterator === null) return
        await closePublicReadinessWatch(iterator, watch.close, iteratorDone)
      }

      return {
        async next(): Promise<IteratorResult<WriteReadinessEvent, undefined>> {
          if (closed) return { done: true, value: undefined }
          try {
            await open()
            if (iterator === null) {
              throw contractError(
                'lifecycle.invariant-violation',
                'connection',
                'public-connection.controls.write-readiness'
              )
            }
            const item = await iterator.next()
            if (item.done) {
              iteratorDone = true
              closed = true
              await close()
              return { done: true, value: undefined }
            }
            const streamItem = item.value
            if (streamItem.kind === 'value') {
              assertPublicConnectionIdentity(
                connection,
                streamItem.value,
                'public-connection.controls.write-readiness.identity'
              )
              return {
                done: false,
                value: Object.freeze({
                  ...controlMetadata(
                    generation,
                    streamItem.value.observedAtMonotonicMs,
                    descriptor,
                    'backend-observation'
                  ),
                  state: 'measured' as const,
                  mode: 'without-response' as const,
                  ready: streamItem.value.ready
                })
              }
            }
            if (streamItem.kind === 'overflow') {
              throw contractError('stream.overflow', 'connection', 'public-connection.controls.write-readiness')
            }
            closed = true
            await close()
            return { done: true, value: undefined }
          } catch (error) {
            const sourceError = rehydratePublicError(error)
            if (closed) throw sourceError
            closed = true
            try {
              await close()
            } catch (cleanupError) {
              throw new AggregateError(
                [sourceError, rehydratePublicError(cleanupError)],
                'BLE readiness watch operation and cleanup both failed'
              )
            }
            throw sourceError
          }
        },
        async return(): Promise<IteratorResult<WriteReadinessEvent, undefined>> {
          closed = true
          try {
            await close()
            return { done: true, value: undefined }
          } catch (error) {
            throw rehydratePublicError(error)
          }
        }
      }
    }
  }
}

async function closePublicReadinessWatch<Attachment extends string>(
  iterator: BoundedAsyncStreamIterator<ConnectionWriteReadinessObservation<Attachment>>,
  close: () => Promise<CleanupRecord>,
  iteratorDone: boolean
): Promise<void> {
  let iteratorError: unknown
  if (!iteratorDone) {
    try {
      if (iterator.return !== undefined) await iterator.return()
    } catch (error) {
      iteratorError = error
    }
  }

  let closeError: unknown
  try {
    const cleanup = await close()
    if (cleanup.state === 'release-failed') {
      closeError = new BleCleanupError(cleanup, 'BLE readiness watch cleanup failed')
    }
  } catch (error) {
    closeError = error
  }

  if (iteratorError !== undefined && closeError !== undefined) {
    throw new AggregateError(
      [rehydratePublicError(iteratorError), rehydratePublicError(closeError)],
      'BLE readiness watch teardown failed'
    )
  }
  if (iteratorError !== undefined) throw rehydratePublicError(iteratorError)
  if (closeError !== undefined) throw rehydratePublicError(closeError)
}

class UnsupportedControlStream<Value> implements AsyncIterable<Value> {
  constructor(
    private readonly operation: string,
    private readonly code: 'capability.unsupported' | 'capability.unavailable'
  ) {}

  [Symbol.asyncIterator](): AsyncIterator<Value> {
    return new UnsupportedControlIterator(this.operation, this.code)
  }
}

class UnsupportedControlIterator<Value> implements AsyncIterator<Value> {
  constructor(
    private readonly operation: string,
    private readonly code: 'capability.unsupported' | 'capability.unavailable'
  ) {}

  async next(): Promise<IteratorResult<Value, undefined>> {
    throw rehydratePublicError(contractError(this.code, 'connection', this.operation))
  }

  async return(): Promise<IteratorResult<Value, undefined>> {
    return { done: true, value: undefined }
  }
}

function createPublicConnectionControls<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: Pick<PublicInternalManager<Attachment, Identity>, 'capability'>,
  connection: PublicControlConnection<Attachment, Identity>,
  generation: string,
  now: () => number
): BleConnectionControls {
  const readRssi = (options: OperationOptions = {}): Promise<RssiObservation> =>
    runPublicControl(async () => {
      const descriptor = requireControlCapability(internal, 'connection:rssi', 'public-connection.controls.read-rssi')
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.readRssi({ signal: normalized.signal, deadline: normalized.deadline })
      if (!Number.isSafeInteger(result.rssi)) {
        throw contractError('protocol.violation', 'connection', 'public-connection.controls.read-rssi')
      }
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'measured' as const,
        rssi: result.rssi
      })
    })

  const effectiveMtu = (): Promise<MtuObservation> =>
    runPublicControl(async () => {
      const descriptor = requireControlCapability(
        internal,
        'connection:effective-mtu',
        'public-connection.controls.effective-mtu'
      )
      const observe = connection.effectiveMtu
      if (observe === undefined) {
        throw contractError('capability.unsupported', 'connection', 'public-connection.controls.effective-mtu')
      }
      const result = await observe()
      assertPublicConnectionIdentity(connection, result, 'public-connection.controls.effective-mtu.identity')
      if (result.attMtu !== null) {
        if (
          !Number.isSafeInteger(result.attMtu) ||
          result.attMtu < MINIMUM_ATT_MTU ||
          result.attMtu > MAXIMUM_REQUESTED_ATT_MTU
        ) {
          throw contractError('protocol.violation', 'connection', 'public-connection.controls.effective-mtu.result')
        }
      }
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-observation'),
        state: result.attMtu === null ? ('unavailable' as const) : ('measured' as const),
        attMtu: result.attMtu,
        payloadBytes: result.payloadBytes,
        platformPduBytes: result.platformPduBytes
      })
    })

  const requestMtu = (requestedMtu: number, options: OperationOptions = {}): Promise<MtuNegotiation> =>
    runPublicControl(async () => {
      if (
        !Number.isSafeInteger(requestedMtu) ||
        requestedMtu < MINIMUM_ATT_MTU ||
        requestedMtu > MAXIMUM_REQUESTED_ATT_MTU
      ) {
        throw contractError('argument.invalid', 'connection', 'public-connection.controls.request-mtu')
      }
      const descriptor = requireControlCapability(
        internal,
        'connection:request-mtu',
        'public-connection.controls.request-mtu'
      )
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.requestMtu(requestedMtu, {
        signal: normalized.signal,
        deadline: normalized.deadline
      })
      if (
        !Number.isSafeInteger(result.negotiatedMtu) ||
        result.negotiatedMtu < MINIMUM_ATT_MTU ||
        result.negotiatedMtu > MAXIMUM_REQUESTED_ATT_MTU
      ) {
        throw contractError('protocol.violation', 'connection', 'public-connection.controls.request-mtu.result')
      }
      const observation = Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'measured' as const,
        attMtu: result.negotiatedMtu,
        payloadBytes: result.negotiatedMtu - 3,
        platformPduBytes: null
      })
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'accepted' as const,
        requestedMtu,
        observation
      })
    })

  const maximumWriteLength = (mode: WriteMode): Promise<MaximumWriteLengthObservation> =>
    runPublicControl(async () => {
      if (mode !== 'with-response' && mode !== 'without-response') {
        throw contractError('argument.invalid', 'connection', 'public-connection.controls.maximum-write-length')
      }
      const descriptor = requireControlCapability(
        internal,
        'gatt:maximum-write-length',
        'public-connection.controls.maximum-write-length'
      )
      const normalized = normalizeOperationOptions({}, now)
      const result = await connection.maximumWriteLength(mode, {
        signal: normalized.signal,
        deadline: normalized.deadline
      })
      if ('connectionId' in result) {
        assertPublicConnectionIdentity(connection, result, 'public-connection.controls.maximum-write-length.identity')
      }
      if (result.mode !== undefined && result.mode !== mode) {
        throw contractError('protocol.violation', 'connection', 'public-connection.controls.maximum-write-length.mode')
      }
      if (!Number.isSafeInteger(result.maximumWriteLength) || result.maximumWriteLength <= 0) {
        throw contractError(
          'protocol.violation',
          'connection',
          'public-connection.controls.maximum-write-length.result'
        )
      }
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-observation'),
        state: 'measured' as const,
        mode,
        maximumWriteLength: result.maximumWriteLength
      })
    })

  const requestPriority = (
    priority: ConnectionPriority,
    options: OperationOptions = {}
  ): Promise<ConnectionPriorityResult> =>
    runPublicControl(async () => {
      if (priority !== 'low-power' && priority !== 'balanced' && priority !== 'high-throughput') {
        throw contractError('argument.invalid', 'connection', 'public-connection.controls.request-priority')
      }
      const descriptor = requireControlCapability(
        internal,
        'connection:priority',
        'public-connection.controls.request-priority'
      )
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.requestPriority(priority, {
        signal: normalized.signal,
        deadline: normalized.deadline
      })
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: result.accepted ? ('accepted' as const) : ('rejected' as const),
        requested: priority
      })
    })

  const readPhy = (options: OperationOptions = {}): Promise<PhyObservation> =>
    runPublicControl(async () => {
      const descriptor = requireControlCapability(internal, 'connection:phy', 'public-connection.controls.read-phy')
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.readPhy({ signal: normalized.signal, deadline: normalized.deadline })
      assertBlePhy(result.txPhy, 'public-connection.controls.read-phy.tx')
      assertBlePhy(result.rxPhy, 'public-connection.controls.read-phy.rx')
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'measured' as const,
        tx: result.txPhy,
        rx: result.rxPhy
      })
    })

  const requestPhy = (preference: PhyPreference, options: OperationOptions = {}): Promise<PhyUpdateResult> =>
    runPublicControl(async () => {
      assertPublicPhyPreference(preference)
      const descriptor = requireControlCapability(internal, 'connection:phy', 'public-connection.controls.request-phy')
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.requestPhy(preference, {
        signal: normalized.signal,
        deadline: normalized.deadline
      })
      if (result.accepted !== (result.observation !== null)) {
        throw contractError('protocol.malformed', 'connection', 'public-connection.controls.request-phy.result')
      }
      if (result.observation !== null) {
        assertBlePhy(result.observation.txPhy, 'public-connection.controls.request-phy.tx')
        assertBlePhy(result.observation.rxPhy, 'public-connection.controls.request-phy.rx')
      }
      const observation =
        result.observation === null
          ? null
          : Object.freeze({
              ...controlMetadata(
                generation,
                result.observation.observedAtMonotonicMs,
                descriptor,
                'backend-observation'
              ),
              state: 'measured' as const,
              tx: result.observation.txPhy,
              rx: result.observation.rxPhy
            })
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: result.accepted ? ('accepted' as const) : ('rejected' as const),
        requested: preference,
        observation
      })
    })

  const unsupportedPromise = <Value>(id: `${string}:${string}`, operation: string): Promise<Value> =>
    runPublicControl(async () => {
      requireControlCapability(internal, id, operation)
      throw contractError('capability.unsupported', 'connection', operation)
    })

  return Object.freeze({
    readRssi,
    effectiveMtu,
    requestMtu,
    maximumWriteLength,
    requestPriority,
    readPhy,
    requestPhy,
    parameters: () =>
      unsupportedPromise<ConnectionParametersObservation>(
        'connection:parameters',
        'public-connection.controls.parameters'
      ),
    parameterEvents: () =>
      unsupportedControlStream<ConnectionParametersObservation>('public-connection.controls.parameter-events'),
    requestSubrate: (_mode: SubrateMode, _options: OperationOptions = {}) =>
      unsupportedPromise<SubrateResult>('connection:subrate', 'public-connection.controls.request-subrate'),
    writeReadiness: (mode: 'without-response') => {
      if (mode !== 'without-response') {
        throw contractError('argument.invalid', 'connection', 'public-connection.controls.write-readiness.mode')
      }
      const descriptor = internal.capability('gatt:write-without-response-readiness')
      if (
        descriptor === null ||
        descriptor.state === 'unsupported' ||
        connection.writeWithoutResponseReadiness === undefined
      ) {
        return unsupportedControlStream<WriteReadinessEvent>('public-connection.controls.write-readiness')
      }
      if (descriptor.state === 'unavailable') {
        return unsupportedControlStream<WriteReadinessEvent>(
          'public-connection.controls.write-readiness',
          'capability.unavailable'
        )
      }
      return publicWriteReadinessStream(connection, generation, descriptor)
    }
  })
}

// Internal factory used by host entrypoints. Hosts derive identity and call this.
export async function createPublicBleManager<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>,
  now: () => number,
  hostOptions: PublicBleManagerHostOptions<Attachment> = {}
): Promise<BleManager> {
  return new PublicBleManager(internal, now, hostOptions)
}

function createInternalPeerIdAuthority<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>
): ((value: string) => PeerId<Attachment>) | null {
  if (!('identity' in internal) || internal.identity === undefined || internal.identity === null) return null
  const attachment = internal.identity.attachment
  const binding = {
    attachmentId: attachment.attachmentId,
    backendInstanceId: attachment.backendInstanceId,
    backendGeneration: attachment.backendGeneration,
    adapterId: attachment.adapter.adapterId,
    adapterGeneration: attachment.adapter.adapterGeneration
  }
  return value => createAttachmentBoundPeerId(binding, value)
}

class PublicBleManager<Attachment extends string, Identity extends BackendIdentity<Attachment>> implements BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics
  readonly peers: BlePeerDirectory
  readonly security: BleSecurity
  private readonly peerIdAuthority: ((value: string) => PeerId<Attachment>) | null
  private readonly activeScanSessions = new Set<{
    readonly controller: PublicScanSessionController<Attachment>
    readonly closeState: () => void
    readonly stop: () => Promise<CleanupRecord>
  }>()

  constructor(
    private readonly internal: PublicInternalManager<Attachment, Identity>,
    private readonly now: () => number,
    hostOptions: PublicBleManagerHostOptions<Attachment>
  ) {
    this.capabilities = new PublicBleCapabilities(internal)
    this.adapter = createPublicAdapter(internal, now)
    this.diagnostics = {
      snapshot: () =>
        Object.freeze({ trace: publicTraceDocument(internal), resourceCounters: this.diagnostics.resourceCounters() }),
      resourceCounters: () => snapshotResourceCounters(publicResourceCounters(internal)),
      startTrace: () => ({ stop: async () => publicTraceDocument(internal) })
    }
    this.peers = hostOptions.peers ?? createPublicPeerDirectory(internal.attachedBackend?.backend?.peers, now)
    this.security = createPublicSecurity(resolveSecurityBackend(internal), this.peers, internal, now)
    this.peerIdAuthority = hostOptions.peerId ?? createInternalPeerIdAuthority(internal)
    const supportsContinuous = typeof internal.supports === 'function' && internal.supports('discovery:continuous-scan')
    this.discovery = Object.freeze({
      kind: hostOptions.discoveryKind ?? (supportsContinuous ? 'continuous-scan' : 'system-chooser')
    })
    this.chooseImpl = hostOptions.choose
  }

  readonly discovery: BleDiscoveryInfo
  private readonly chooseImpl: ((options: ChooseOptions) => Promise<BlePeer>) | undefined

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    try {
      assertPublicScanOptions(options)
      const { signal, deadline } = normalizeOperationOptions(options, this.now)
      const delivery = resolveStreamPolicy(options.delivery ?? 'balanced')
      const normalizedQuery = normalizeScanQuery(options.query)
      const reportLostAfterMs = options.observation?.reportLostAfterMs
      if (options.observation?.includeRawAdvertisement === true) {
        throw contractError('capability.unsupported', 'scan', 'public-ble-manager.scan.raw-advertisement')
      }
      if (options.platform !== undefined) {
        throw contractError('capability.unsupported', 'scan', 'public-ble-manager.scan.platform-options')
      }
      if (options.duplicates === 'all' && reportLostAfterMs !== undefined) {
        throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.duplicates')
      }
      if (reportLostAfterMs !== undefined && typeof this.internal.scheduleDeadline !== 'function') {
        throw contractError('capability.unavailable', 'scan', 'public-ble-manager.scan.report-lost-after')
      }
      const plan = typeof this.internal.planScan === 'function' ? this.internal.planScan(normalizedQuery) : null
      const internalOptions: InternalScanOptions<Attachment, string> = {
        query: normalizedQuery,
        plan: plan ?? undefined,
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
        duplicatePolicy: 'all',
        timestampPolicy: 'source-then-receipt',
        delivery: {
          itemCapacity: delivery.itemCapacity,
          byteCapacity: delivery.byteCapacity,
          reservedControlCapacity: delivery.reservedControlCapacity,
          overflowPolicy: delivery.overflowPolicy
        },
        deadline,
        signal,
        sharing: { mode: 'owner', allowSharing: false }
      }
      const session = await this.internal.scan(internalOptions)
      const scanState = createScanState()
      scanState.emit({ state: 'active' })
      const stopState: {
        viewReleased: boolean
        nativeReleased: boolean
        stopPromise: Promise<CleanupRecord> | null
        pendingCleanupError: unknown | null
      } = {
        viewReleased: false,
        nativeReleased: false,
        stopPromise: null,
        pendingCleanupError: null
      }
      let stopScan: (reason: PublicScanEventTerminalReason) => Promise<CleanupRecord> = async () => ({
        state: 'released',
        failures: []
      })
      const controller = new PublicScanSessionController<Attachment>(
        session.observations,
        normalizedQuery,
        options.duplicates ?? 'coalesced',
        delivery,
        this.now,
        (deadlineAt, action) => scheduleInternalScanDeadline(this.internal, deadlineAt, action),
        reportLostAfterMs,
        reason => {
          stopScan(reason).catch(error => {
            stopState.pendingCleanupError = error
            scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
          })
        }
      )
      stopScan = async (reason: PublicScanEventTerminalReason): Promise<CleanupRecord> => {
        if (stopState.stopPromise !== null) return stopState.stopPromise
        scanState.emit({ state: 'stopping' })
        const run = (async () => {
          const phases: { readonly error?: unknown; readonly cleanup?: CleanupRecord }[] = []
          if (stopState.pendingCleanupError !== null) {
            phases.push({ error: stopState.pendingCleanupError })
          }
          if (!stopState.viewReleased) {
            try {
              const view = await controller.closeView(reason)
              if (view.state === 'released') stopState.viewReleased = true
              phases.push({ cleanup: view })
            } catch (error) {
              phases.push({ error })
            }
          }
          if (!stopState.nativeReleased) {
            try {
              const native = await rehydratePublicPromise(session.stop())
              if (native.state === 'released') stopState.nativeReleased = true
              phases.push({ cleanup: native })
            } catch (error) {
              phases.push({ error })
            }
          }
          try {
            const combined = collectCleanupPhases(phases)
            if (stopState.viewReleased && stopState.nativeReleased) {
              scanState.emit({ state: 'stopped' })
              scanState.close()
              this.activeScanSessions.delete(activeScan)
              stopState.pendingCleanupError = null
            } else {
              scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
              scanState.close()
              stopState.stopPromise = null
            }
            return combined
          } catch (error) {
            scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
            stopState.stopPromise = null
            throw error
          }
        })()
        stopState.stopPromise = run
        return run
      }
      const activeScan = { controller, closeState: scanState.close, stop: () => stopScan('owner-released') }
      this.activeScanSessions.add(activeScan)
      const publicSession: ScanSession = {
        plan,
        stop: () => stopScan('owner-released'),
        observations: controller.observations,
        events: controller.events,
        state: scanState.stream
      }
      publicScanFingerprintInspectors.set(publicSession, () => controller.fingerprintAccounting())
      return publicSession
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async find(options: FindOptions = {}): Promise<BlePeer> {
    const { select, ...scanOptions } = options
    const operation = normalizeOperationOptions(options, this.now)
    const scan = await this.scan({
      ...scanOptions,
      duplicates: 'coalesced',
      delivery: 'latest',
      timeoutMs: options.timeoutMs ?? 10_000
    })
    return runWithCleanup(
      () => findPeerInScan(scan, select, { ...operation, now: this.now }),
      () => scan.stop()
    )
  }

  async choose(options: ChooseOptions = {}): Promise<BlePeer> {
    try {
      assertPublicChooseOptions(options)
      if (this.chooseImpl === undefined) {
        throw contractError('capability.unsupported', 'chooser', 'public-ble-manager.choose')
      }
      return await this.chooseImpl(options)
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async connect(peer: BlePeer | string | PeerReference, options: ConnectOptions = {}): Promise<BleConnection> {
    try {
      assertPublicConnectOptions(options)
      const { signal, deadline } = normalizeOperationOptions(options, this.now)
      const intent = options.intent ?? 'direct'
      assertDirectConnectionCapability(
        this.internal.capability('connection:direct'),
        'public-ble-manager.connect.direct'
      )
      if (intent === 'when-available' && !this.internal.supports('connection:when-available')) {
        throw contractError('capability.unsupported', 'connection', 'public-ble-manager.connect.when-available')
      }
      if (options.preferredPhy !== undefined && !this.internal.supports('connection:phy')) {
        throw contractError('capability.unsupported', 'connection', 'public-ble-manager.connect.preferred-phy')
      }
      if (isReferenceLike(peer) && !isPeerReference(peer)) {
        throw contractError('peer.reference-invalid', 'connection', 'public-ble-manager.connect-reference')
      }
      const resolvedPeer = isPeerReference(peer) ? await this.peers.resolve(peer, options) : peer
      if (resolvedPeer === null)
        throw rehydratePublicError(
          contractError('peer.not-found', 'connection', 'public-ble-manager.connect-reference')
        )
      const peerIdString = typeof resolvedPeer === 'string' ? resolvedPeer : resolvedPeer.id
      const peerId = this.peerIdAuthority?.(peerIdString)
      if (peerId === undefined) {
        throw contractError('capability.unavailable', 'connection', 'public-ble-manager.connect.peer-id-authority')
      }
      const internalConnection = await this.internal.connect(peerId, {
        signal,
        deadline,
        intent,
        transport: options.transport,
        preferredPhy: options.preferredPhy
      })
      const publicPeer =
        typeof resolvedPeer === 'string'
          ? snapshotBlePeer({ id: peerIdString, name: null, rssi: null })
          : snapshotBlePeer(resolvedPeer)
      return {
        peer: publicPeer,
        connectionGeneration: String(internalConnection.connectionGeneration),
        lifecycleEvents: publicConnectionEvents(internalConnection.events),
        controls: createPublicConnectionControls(
          this.internal,
          internalConnection,
          String(internalConnection.connectionGeneration),
          this.now
        ),
        discover: async (discoverOptions: OperationOptions = {}) => {
          try {
            const normalized = normalizeOperationOptions(discoverOptions, this.now)
            const source = await internalConnection.discover({
              signal: normalized.signal,
              deadline: normalized.deadline
            })
            return createPublicGattDatabase(source)
          } catch (error) {
            throw rehydratePublicError(error)
          }
        },
        rediscoverGatt: async (rediscoverOptions: RediscoverGattOptions) => {
          try {
            if (rediscoverOptions.reason !== 'service-changed' && rediscoverOptions.reason !== 'manual') {
              throw contractError('argument.invalid', 'gatt', 'public-connection.rediscover-gatt.reason')
            }
            const normalized = normalizeOperationOptions(rediscoverOptions, this.now)
            const source = await internalConnection.rediscoverGatt(
              {
                signal: normalized.signal,
                deadline: normalized.deadline
              },
              rediscoverOptions.reason === 'manual' ? 'manual-rediscovery' : 'service-changed'
            )
            return createPublicGattDatabase(source)
          } catch (error) {
            throw rehydratePublicError(error)
          }
        },
        disconnect: () => rehydratePublicPromise(internalConnection.disconnect()),
        release: () => rehydratePublicPromise(internalConnection.release())
      }
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async withConnection<T>(
    peer: BlePeer | string | PeerReference,
    options: ConnectOptions,
    action: (connection: BleConnection) => Promise<T>
  ): Promise<T> {
    const connection = await this.connect(peer, options)
    return runWithCleanup(
      () => action(connection),
      () => connection.release()
    )
  }

  async withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T> {
    const scan = await this.scan(options)
    return runWithCleanup(
      () => action(scan),
      () => scan.stop()
    )
  }

  async withDiscoveredConnection<T>(
    peer: BlePeer | string | PeerReference,
    options: ConnectOptions,
    action: (scope: { readonly connection: BleConnection; readonly gatt: GattDatabase }) => Promise<T>
  ): Promise<T> {
    const normalized = normalizeOperationOptions(options, this.now)
    return this.withConnection(peer, options, async connection => {
      if (normalized.deadline !== null && this.now() >= normalized.deadline) {
        throw contractError('operation.timed-out', 'connection', 'public-ble-manager.with-discovered-connection')
      }
      const remainingMs =
        normalized.deadline === null ? undefined : Math.max(1, Math.trunc(normalized.deadline - this.now()))
      const gatt = await connection.discover({
        signal: options.signal,
        ...(remainingMs === undefined ? {} : { timeoutMs: remainingMs })
      })
      return action(Object.freeze({ connection, gatt }))
    })
  }

  async destroy(): Promise<CleanupRecord> {
    try {
      const active = [...this.activeScanSessions]
      const viewResults: { readonly error?: unknown; readonly cleanup?: CleanupRecord }[] = []
      await Promise.all(
        active.map(async scan => {
          try {
            const cleanup = await scan.stop()
            viewResults.push({ cleanup })
          } catch (error) {
            viewResults.push({ error })
          }
        })
      )
      let cleanup: CleanupRecord | undefined
      let nativeError: unknown
      try {
        cleanup = await this.internal.destroy()
      } catch (error) {
        nativeError = error
      }
      return collectCleanupPhases([
        ...viewResults,
        ...(nativeError === undefined ? [] : [{ error: nativeError }]),
        ...(cleanup === undefined ? [] : [{ cleanup }])
      ])
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
}

function publicTraceDocument<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>
): ReturnType<PublicInternalManager<Attachment, Identity>['traceDocument']> {
  return internal.attachedBackend?.backend.traceDocument?.() ?? internal.traceDocument()
}

function resolveSecurityBackend<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>
): import('../backend-contract/security').SecurityBackend | undefined {
  if (typeof internal.securityBackend === 'function') return internal.securityBackend()
  return internal.attachedBackend?.backend?.security
}

function publicResourceCounters<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>
): Record<keyof ResourceCounters, number> {
  const core = internal.localResourceCounters()
  const backend = internal.attachedBackend?.backend.resourceCounters()
  const value = (key: keyof ResourceCounters): number => Number(backend?.[key] ?? core[key])
  return {
    activeScanControllers: value('activeScanControllers'),
    scanConsumers: value('scanConsumers'),
    chooserSessions: value('chooserSessions'),
    connectionLeases: value('connectionLeases'),
    physicalLinks: value('physicalLinks'),
    databaseSnapshots: value('databaseSnapshots'),
    physicalCccdEnablements: value('physicalCccdEnablements'),
    subscriptionConsumers: value('subscriptionConsumers'),
    queuedOperations: value('queuedOperations'),
    dispatchedOperations: value('dispatchedOperations'),
    retainedByteBuffers: value('retainedByteBuffers'),
    restorationRecords: value('restorationRecords'),
    orphanedIpcOwners: value('orphanedIpcOwners')
  }
}

// Re-export for host factories that need the internal type.
export type { BleManagerOptions }

function createPublicAdapter<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>,
  now: () => number
): BleAdapter {
  const identity = internal.identity
  const adapterId = identity?.attachment?.adapter?.adapterId
  return {
    id: typeof adapterId === 'string' ? adapterId : null,
    state: async () => snapshotPublicAdapterState(await internal.adapterState()),
    waitUntilReady: options => waitForPublicAdapter(internal, now, options),
    watchState: options => watchPublicAdapter(internal, now, options)
  }
}

async function watchPublicAdapter<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>,
  now: () => number,
  options: AdapterWatchOptions = {}
): Promise<BleAdapterStateWatch> {
  try {
    const signal = normalizeOperationOptions({ signal: options.signal ?? undefined }, now).signal
    const watch = await internal.adapterStates({ signal })
    if (signal?.aborted === true) {
      await watch.stop()
      throw contractError('operation.aborted', 'adapter', 'public-adapter.watch-state')
    }
    let stopPromise: Promise<CleanupRecord> | null = null
    const abortHandler = () => {
      stop().catch(() => undefined)
    }
    const stop = (): Promise<CleanupRecord> => {
      if (stopPromise !== null) return stopPromise
      const result = watch.stop().then(
        cleanup => {
          signal?.removeEventListener('abort', abortHandler)
          if (cleanup.state === 'release-failed') stopPromise = null
          return cleanup
        },
        error => {
          signal?.removeEventListener('abort', abortHandler)
          stopPromise = null
          throw error
        }
      )
      stopPromise = result
      return result
    }
    signal?.addEventListener('abort', abortHandler, { once: true })
    return Object.freeze({
      initial: snapshotPublicAdapterState(watch.initial),
      values: mapPublicAdapterStates(watch.values),
      stop
    })
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function mapPublicAdapterStates<Attachment extends string>(
  source: BoundedAsyncStream<AdapterStateSnapshot<Attachment>>
): AsyncIterable<StreamItem<BleAdapterState>> {
  return {
    [Symbol.asyncIterator](): BoundedAsyncStreamIterator<BleAdapterState> {
      const iterator = source[Symbol.asyncIterator]()
      const mapped: BoundedAsyncStreamIterator<BleAdapterState> = {
        async next() {
          const result = await iterator.next()
          if (result.done) return { done: true, value: undefined }
          if (result.value.kind !== 'value') return { done: false, value: result.value }
          return { done: false, value: { kind: 'value', value: snapshotPublicAdapterState(result.value.value) } }
        },
        return: () => iterator.return(),
        [Symbol.asyncIterator]() {
          return mapped
        }
      }
      return mapped
    }
  }
}

async function waitForPublicAdapter<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  internal: PublicInternalManager<Attachment, Identity>,
  now: () => number,
  options: AdapterReadinessOptions = {}
): Promise<BleAdapterState> {
  try {
    const normalized = normalizeOperationOptions(options, now)
    const deadline = normalized.deadline ?? now() + 10_000
    const watch = await internal.adapterStates({ signal: normalized.signal })
    const iterator = watch.values[Symbol.asyncIterator]()
    return await runWithCleanup(
      async () => {
        let current = watch.initial
        while (true) {
          assertAdapterCanBecomeReady(current, options.operation ?? 'scan')
          if (adapterIsReady(current)) return snapshotPublicAdapterState(current)
          if (normalized.signal?.aborted === true)
            throw contractError('operation.aborted', 'adapter', 'public-adapter.wait-until-ready')
          if (now() >= deadline)
            throw contractError('operation.timed-out', 'adapter', 'public-adapter.wait-until-ready')
          const item = await nextAdapterState(iterator, deadline - now())
          if (item.done) throw contractError('stream.closed', 'adapter', 'public-adapter.wait-until-ready')
          if (item.value.kind === 'terminal')
            throw contractError('stream.closed', 'adapter', 'public-adapter.wait-until-ready')
          if (item.value.kind === 'overflow') continue
          current = item.value.value
        }
      },
      () => stopAdapterWatch(iterator, watch.stop)
    )
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

async function stopAdapterWatch(
  iterator: AsyncIterator<unknown>,
  stop: () => Promise<CleanupRecord>
): Promise<CleanupRecord> {
  const failures: unknown[] = []
  try {
    if (iterator.return !== undefined) await iterator.return()
  } catch (error) {
    failures.push(error)
  }
  let cleanup: CleanupRecord
  try {
    cleanup = await stop()
  } catch (error) {
    failures.push(error)
    throw new AggregateError(failures, 'BLE adapter watch cleanup failed')
  }
  if (cleanup.state === 'release-failed') failures.push(new BleCleanupError(cleanup))
  if (failures.length > 0) throw new AggregateError(failures, 'BLE adapter watch cleanup failed')
  return cleanup
}

function adapterIsReady<Attachment extends string>(state: AdapterStateSnapshot<Attachment>): boolean {
  return state.availability === 'available' && state.power === 'on' && !isAuthorizationBlocking(state.authorization)
}

function assertAdapterCanBecomeReady<Attachment extends string>(
  state: AdapterStateSnapshot<Attachment>,
  operation: string
): void {
  if (state.availability === 'unsupported' || state.power === 'unsupported') {
    throw contractError('capability.unsupported', 'adapter', `public-adapter.${operation}`)
  }
  if (state.authorization === 'denied')
    throw contractError('permission.denied', 'adapter', `public-adapter.${operation}`)
  if (state.authorization === 'restricted')
    throw contractError('permission.restricted', 'adapter', `public-adapter.${operation}`)
  if (state.authorization === 'unavailable')
    throw contractError('permission.denied', 'adapter', `public-adapter.${operation}`)
}

async function nextAdapterState<Attachment extends string>(
  iterator: import('../backend-contract/streams').BoundedAsyncStreamIterator<AdapterStateSnapshot<Attachment>>,
  timeoutMs: number
): Promise<
  IteratorResult<import('../backend-contract/streams').StreamItem<AdapterStateSnapshot<Attachment>>, undefined>
> {
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    return await Promise.race([
      iterator.next(),
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(contractError('operation.timed-out', 'adapter', 'public-adapter.wait-until-ready')),
          timeoutMs
        )
      })
    ])
  } finally {
    if (timer !== undefined) clearTimeout(timer)
  }
}

function snapshotPublicAdapterState<Attachment extends string>(
  state: AdapterStateSnapshot<Attachment>
): BleAdapterState {
  return Object.freeze({
    availability: state.availability,
    authorization: state.authorization,
    power: state.power,
    backendGeneration: String(state.backendGeneration),
    updatedAt: Number(state.updatedAt),
    safeReason: state.safeReason
  })
}

export function filterScanObservations(
  source: BoundedAsyncStream<AdvertisementObservation<string> | IpcAdvertisement>,
  query: ReturnType<typeof normalizeScanQuery>,
  duplicates: 'coalesced' | 'all' = 'all'
): BoundedAsyncStream<PublicScanObservation> {
  return {
    limits: source.limits,
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator](): BoundedAsyncStreamIterator<PublicScanObservation> {
      const iterator = source[Symbol.asyncIterator]()
      const lastObservations = new Map<string, string>()
      return {
        async next() {
          while (true) {
            const item = await iterator.next()
            if (item.done) {
              lastObservations.clear()
              return item
            }
            if (item.value.kind === 'overflow' || item.value.kind === 'terminal') {
              return { done: false, value: item.value }
            }
            const observation = projectPublicScanObservation(item.value.value)
            if (observationMatchesScanQuery(query, observation)) {
              if (duplicates === 'coalesced') {
                const fingerprint = publicObservationFingerprint(observation)
                if (lastObservations.get(observation.peer.id) === fingerprint) continue
                if (lastObservations.size >= 256) {
                  const oldest = lastObservations.keys().next().value
                  if (oldest !== undefined) lastObservations.delete(oldest)
                }
                lastObservations.set(observation.peer.id, fingerprint)
              }
              return { done: false, value: { kind: 'value', value: observation } }
            }
          }
        },
        return: async () => {
          lastObservations.clear()
          await iterator.return()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    },
    close: () => source.close()
  }
}

function publicObservationFingerprint(observation: PublicScanObservation): string {
  const bytes = (value: Readonly<Uint8Array>): readonly number[] => [...value]
  return JSON.stringify({
    peerReference: observation.peerReference === undefined ? null : encodePeerReference(observation.peerReference),
    localName: observation.localName,
    rssi: observation.rssi,
    connectable: observation.connectable,
    serviceUuids: observation.serviceUuids,
    manufacturerData:
      observation.manufacturerData?.map(entry => ({ companyId: entry.companyId, data: bytes(entry.data) })) ?? null,
    serviceData: observation.serviceData?.map(entry => ({ service: entry.service, data: bytes(entry.data) })) ?? null
  })
}

function estimatePublicScanObservationBytes(observation: PublicScanObservation): number {
  let bytes = 128
  for (const entry of observation.manufacturerData ?? []) bytes += entry.data.byteLength
  for (const entry of observation.serviceData ?? []) bytes += entry.data.byteLength
  return bytes
}

function estimatePublicDiscoveryEventBytes(event: DiscoveryEvent): number {
  const peerBytes = event.peer.id.length * 2 + (event.peer.name?.length ?? 0) * 2
  return event.kind === 'observed' ? 32 + peerBytes : 96 + peerBytes
}

export function publicConnectionEvents(
  source: BoundedAsyncStream<ConnectionLifecycleEvent<string>>
): AsyncIterable<BleConnectionEvent> {
  return broadcastConnectionEvents(mapPublicConnectionEvents(source))
}

export function broadcastConnectionEvents(
  source: AsyncIterable<BleConnectionEvent>
): AsyncIterable<BleConnectionEvent> {
  return new PublicConnectionEventBroadcast(source)
}

function mapPublicConnectionEvents(
  source: BoundedAsyncStream<ConnectionLifecycleEvent<string>>
): AsyncIterable<BleConnectionEvent> {
  return {
    [Symbol.asyncIterator]() {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next(): Promise<IteratorResult<BleConnectionEvent, undefined>> {
          while (true) {
            const item = await iterator.next()
            if (item.done) return { done: true, value: undefined }
            if (item.value.kind !== 'value') {
              if (item.value.kind === 'terminal') return { done: true, value: undefined }
              throw contractError('stream.overflow', 'connection', 'public-connection.events')
            }
            const event = item.value.value
            return {
              done: false,
              value: Object.freeze({
                kind: event.kind,
                previous: event.previous,
                current: event.current,
                cause: event.cause,
                connectionGeneration: String(event.connectionGeneration),
                sequence: event.sequence
              })
            }
          }
        },
        return: async () => {
          await iterator.return()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

class PublicConnectionEventBroadcast implements AsyncIterable<BleConnectionEvent> {
  private readonly subscribers = new Set<CoreBoundedStream<BleConnectionEvent>>()
  private pumping = false
  private terminalReason: 'closed' | 'source-failed' | null = null

  constructor(private readonly source: AsyncIterable<BleConnectionEvent>) {}

  [Symbol.asyncIterator](): AsyncIterator<BleConnectionEvent> {
    const stream = new CoreBoundedStream<BleConnectionEvent>(
      { itemCapacity: capacity(64), byteCapacity: capacity(64 * 1024), reservedControlCapacity: capacity(1) },
      'error'
    )
    if (this.terminalReason === null) {
      this.subscribers.add(stream)
      this.startPump()
    } else {
      stream.closeWithReason(this.terminalReason)
    }
    const iterator = stream[Symbol.asyncIterator]()
    return {
      next: async () => {
        const item = await iterator.next()
        if (item.done) return { done: true, value: undefined }
        if (item.value.kind === 'value') return { done: false, value: item.value.value }
        if (item.value.kind === 'overflow') {
          throw contractError('stream.overflow', 'connection', 'public-connection.events')
        }
        return { done: true, value: undefined }
      },
      return: async () => {
        this.subscribers.delete(stream)
        await iterator.return()
        return { done: true, value: undefined }
      }
    }
  }

  private startPump(): void {
    if (this.pumping) return
    this.pumping = true
    this.pump().catch(() => undefined)
  }

  private async pump(): Promise<void> {
    try {
      for await (const event of this.source) {
        for (const subscriber of [...this.subscribers]) {
          const result = subscriber.emit(event, 512)
          if (result.terminated) this.subscribers.delete(subscriber)
        }
      }
      this.terminalReason = 'closed'
      this.closeSubscribers('closed')
    } catch {
      this.terminalReason = 'source-failed'
      this.closeSubscribers('source-failed')
    }
  }

  private closeSubscribers(reason: 'closed' | 'source-failed'): void {
    for (const subscriber of this.subscribers) {
      subscriber.closeWithReason(reason)
      this.subscribers.delete(subscriber)
    }
  }
}

export function peerFromPublicObservation(
  observation: PublicScanObservation | AdvertisementObservation<string> | IpcAdvertisement
): BlePeer {
  return 'peer' in observation ? observation.peer : projectPublicScanObservation(observation).peer
}

function projectPublicScanObservation<Attachment extends string>(
  observation: AdvertisementObservation<Attachment> | IpcAdvertisement
): PublicScanObservation {
  const normalized = normalizeScanObservation(observation)
  const isCompact = 'peerId' in observation
  const id = isCompact ? observation.peerId : String(observation.device.id)
  const name = isCompact
    ? observation.localName
    : observation.localName.state === 'present'
      ? observation.localName.value
      : null
  const rssi = isCompact ? observation.rssi : observation.rssi.state === 'present' ? observation.rssi.value : null
  const peer = snapshotBlePeer({
    id,
    name,
    rssi,
    reference: normalized.peerReference ?? null,
    sources: ['scan-observed'],
    lastAdvertisement: normalized
  })
  const observedAtMonotonicMs = isCompact ? null : Number(observation.receivedAtMonotonicMs)
  return Object.freeze({ ...normalized, peer, observedAtMonotonicMs })
}

function isReferenceLike(value: unknown): value is object {
  return (
    typeof value === 'object' &&
    value !== null &&
    ('version' in value || 'backendId' in value || 'scope' in value || 'opaqueId' in value)
  )
}

export function assertPublicScanOptions(options: ScanOptions): void {
  if (typeof options !== 'object' || options === null || Array.isArray(options)) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.options')
  }
  const allowed = new Set(['signal', 'timeoutMs', 'query', 'duplicates', 'delivery', 'observation', 'platform'])
  if (Object.keys(options).some(key => !allowed.has(key))) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.options')
  }
  if (options.duplicates !== undefined && options.duplicates !== 'coalesced' && options.duplicates !== 'all') {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.duplicates')
  }
  if (
    options.delivery !== undefined &&
    (typeof options.delivery === 'object'
      ? options.delivery.preset !== 'custom'
      : options.delivery !== 'latest' && options.delivery !== 'balanced' && options.delivery !== 'lossless-bounded')
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.delivery')
  }
  if (
    typeof options.delivery === 'object' &&
    (options.delivery.budget === undefined ||
      !Number.isSafeInteger(options.delivery.budget.itemCapacity) ||
      options.delivery.budget.itemCapacity <= 0 ||
      !Number.isSafeInteger(options.delivery.budget.byteCapacity) ||
      options.delivery.budget.byteCapacity <= 0)
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.delivery.budget')
  }
  if (
    options.observation !== undefined &&
    (typeof options.observation !== 'object' ||
      options.observation === null ||
      Array.isArray(options.observation) ||
      Object.keys(options.observation).some(key => key !== 'reportLostAfterMs' && key !== 'includeRawAdvertisement'))
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.observation')
  }
  if (
    options.observation?.reportLostAfterMs !== undefined &&
    (typeof options.observation.reportLostAfterMs !== 'number' ||
      !Number.isSafeInteger(options.observation.reportLostAfterMs) ||
      options.observation.reportLostAfterMs <= 0 ||
      options.observation.reportLostAfterMs > 2_147_483_647)
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.report-lost-after')
  }
  if (
    options.observation?.includeRawAdvertisement !== undefined &&
    typeof options.observation.includeRawAdvertisement !== 'boolean'
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.raw-advertisement')
  }
  assertPublicScanPlatformOptions(options.platform)
}

function assertPublicScanPlatformOptions(options: ScanPlatformOptions | undefined): void {
  if (options === undefined) return
  if (typeof options !== 'object' || options === null || !('kind' in options)) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.platform-options')
  }
  if (options.kind === 'android') {
    if (
      Object.keys(options).some(
        key => !['kind', 'mode', 'callbackType', 'reportDelayMs', 'legacy', 'phy'].includes(key)
      ) ||
      (options.mode !== undefined &&
        !['low-power', 'balanced', 'low-latency', 'opportunistic'].includes(options.mode)) ||
      (options.callbackType !== undefined &&
        !['all-matches', 'first-match', 'match-lost'].includes(options.callbackType)) ||
      (options.reportDelayMs !== undefined &&
        (!Number.isSafeInteger(options.reportDelayMs) ||
          options.reportDelayMs < 0 ||
          options.reportDelayMs > 2_147_483_647)) ||
      (options.legacy !== undefined && typeof options.legacy !== 'boolean') ||
      (options.phy !== undefined && !['all-supported', '1m', 'coded'].includes(options.phy))
    ) {
      throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.platform-options')
    }
    return
  }
  if (
    options.kind !== 'corebluetooth' &&
    options.kind !== 'winrt' &&
    options.kind !== 'web' &&
    options.kind !== 'electron' &&
    options.kind !== 'tauri'
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.platform-options')
  }
  if (Object.keys(options).length !== 1) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.platform-options')
  }
}

export function assertPublicConnectOptions(options: ConnectOptions): void {
  if (typeof options !== 'object' || options === null || Array.isArray(options)) {
    throw contractError('argument.invalid', 'connection', 'public-ble-manager.connect.options')
  }
  const allowed = new Set(['signal', 'timeoutMs', 'intent', 'transport', 'preferredPhy'])
  if (Object.keys(options).some(key => !allowed.has(key))) {
    throw contractError('argument.invalid', 'connection', 'public-ble-manager.connect.options')
  }
  if (options.intent !== undefined && options.intent !== 'direct' && options.intent !== 'when-available') {
    throw contractError('argument.invalid', 'connection', 'public-ble-manager.connect.intent')
  }
  if (options.transport !== undefined && options.transport !== 'le' && options.transport !== 'auto') {
    throw contractError('argument.invalid', 'connection', 'public-ble-manager.connect.transport')
  }
  if (options.preferredPhy !== undefined) {
    if (
      !Array.isArray(options.preferredPhy) ||
      options.preferredPhy.length === 0 ||
      options.preferredPhy.some(phy => phy !== 'le-1m' && phy !== 'le-2m' && phy !== 'le-coded')
    ) {
      throw contractError('argument.invalid', 'connection', 'public-ble-manager.connect.preferred-phy')
    }
  }
}

function assertPublicPhyPreference(preference: PhyPreference): void {
  if (
    typeof preference !== 'object' ||
    preference === null ||
    Array.isArray(preference) ||
    Object.keys(preference).some(key => key !== 'tx' && key !== 'rx') ||
    (preference.tx === undefined && preference.rx === undefined) ||
    (preference.tx !== undefined && !isPublicBlePhy(preference.tx)) ||
    (preference.rx !== undefined && !isPublicBlePhy(preference.rx))
  ) {
    throw contractError('argument.invalid', 'connection', 'public-connection.controls.request-phy.preference')
  }
}

function isPublicBlePhy(value: string): value is BlePhy {
  return value === 'le-1m' || value === 'le-2m' || value === 'le-coded'
}

function assertBlePhy(value: unknown, operation: string): asserts value is BlePhy | null {
  if (value !== null && (typeof value !== 'string' || !isPublicBlePhy(value))) {
    throw contractError('protocol.violation', 'connection', operation)
  }
}

export function assertPublicChooseOptions(options: ChooseOptions): void {
  const allowed = new Set(['signal', 'timeoutMs', 'filters', 'optionalServices', 'acceptAllDevices'])
  if (Object.keys(options).some(key => !allowed.has(key))) {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.options')
  }
  if (options.filters !== undefined && !Array.isArray(options.filters)) {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filters')
  }
  if (options.filters !== undefined) {
    for (const filter of options.filters) {
      if (typeof filter !== 'object' || filter === null || Array.isArray(filter)) {
        throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter')
      }
      const allowedFilterKeys = new Set(['serviceUuids', 'manufacturerData', 'localNamePrefix'])
      if (Object.keys(filter).some(key => !allowedFilterKeys.has(key))) {
        throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter.options')
      }
      if (filter.serviceUuids !== undefined) {
        if (!Array.isArray(filter.serviceUuids)) {
          throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter.services')
        }
        for (const uuid of filter.serviceUuids) assertChooseUuid(uuid)
      }
      if (filter.localNamePrefix !== undefined && typeof filter.localNamePrefix !== 'string') {
        throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter.name-prefix')
      }
      if (filter.manufacturerData !== undefined) {
        if (!Array.isArray(filter.manufacturerData)) {
          throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter.manufacturer-data')
        }
        for (const manufacturer of filter.manufacturerData) {
          if (
            typeof manufacturer !== 'object' ||
            manufacturer === null ||
            !Number.isSafeInteger(manufacturer.companyIdentifier) ||
            manufacturer.companyIdentifier < 0 ||
            (manufacturer.dataPrefix !== undefined && !(manufacturer.dataPrefix instanceof Uint8Array))
          ) {
            throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.filter.manufacturer-entry')
          }
        }
      }
      const hasServiceCriterion = filter.serviceUuids !== undefined && filter.serviceUuids.length > 0
      const hasManufacturerCriterion = filter.manufacturerData !== undefined && filter.manufacturerData.length > 0
      const hasNameCriterion = filter.localNamePrefix !== undefined && filter.localNamePrefix.length > 0
      if (!hasServiceCriterion && !hasManufacturerCriterion && !hasNameCriterion) {
        throw contractError('scan.filter-invalid', 'chooser', 'public-ble-manager.choose.filter-empty')
      }
    }
  }
  if (options.optionalServices !== undefined && !Array.isArray(options.optionalServices)) {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.optional-services')
  }
  if (options.optionalServices !== undefined) {
    for (const uuid of options.optionalServices) assertChooseUuid(uuid)
  }
  if (options.acceptAllDevices !== undefined && typeof options.acceptAllDevices !== 'boolean') {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.accept-all-devices')
  }
}

function assertChooseUuid(value: unknown): void {
  if (!(typeof value === 'string' || typeof value === 'number')) {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.uuid')
  }
  try {
    canonicalUuid(typeof value === 'number' ? value.toString(16) : value)
  } catch {
    throw contractError('argument.invalid', 'chooser', 'public-ble-manager.choose.uuid')
  }
}

export async function findPeerInScan(
  scan: ScanSession,
  select: FindOptions['select'],
  operation: {
    readonly signal: AbortSignal | null
    readonly deadline: number | null
    readonly now: () => number
  } | null = null
): Promise<BlePeer> {
  const iterator = scan.observations[Symbol.asyncIterator]()
  while (true) {
    const item = await iterator.next()
    if (item.done) throw rehydratePublicError(contractError('operation.timed-out', 'scan', 'public-ble-manager.find'))
    if (item.value.kind === 'terminal') {
      if (item.value.reason === 'operation-timed-out') {
        throw rehydratePublicError(contractError('operation.timed-out', 'scan', 'public-ble-manager.find'))
      }
      if (operation !== null && operation.signal !== null && operation.signal.aborted) {
        throw rehydratePublicError(contractError('operation.aborted', 'scan', 'public-ble-manager.find'))
      }
      if (operation !== null && operation.deadline !== null && operation.deadline <= operation.now()) {
        throw rehydratePublicError(contractError('operation.timed-out', 'scan', 'public-ble-manager.find'))
      }
      throw rehydratePublicError(contractError('stream.closed', 'scan', 'public-ble-manager.find'))
    }
    if (item.value.kind === 'overflow') {
      throw rehydratePublicError(contractError('stream.overflow', 'scan', 'public-ble-manager.find'))
    }
    const peer = peerFromPublicObservation(item.value.value)
    if (select === undefined || select === 'first' || select(peer)) return peer
  }
}
