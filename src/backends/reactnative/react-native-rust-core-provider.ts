// src/backends/reactnative/react-native-rust-core-provider.ts
//
// F01 React Native binding-backed provider: manager creation, scan, connect,
// subscribe, timeout, and dispose execute the native Rust core — never the
// TypeScript 4.0 runtime.
//
// Every BLE data-path operation dispatches through an admitted
// `ReactNativeRustCoreSession` (see `./react-native-rust-core`): the op name
// and args cross verbatim via `session.invoke` and the raw core result
// returns. This mirrors the Node NAPI dispatch pattern
// (`bindings/napi/src/dispatch.rs`, where `UbmCentral` owns one
// `DesktopCentral`): TypeScript schedules nothing, owns no subscription
// state, no retry policy, and no timeout timers. Deadlines arrive as
// `timeoutMs` op args so the core owns the caller outcome; abort signals map
// to core `op.cancel` dispatches, never to a TS-side timer.
//
// Binding op contract (the exact F01 slice a JNI/UniFFI native module must
// implement; the F01 acceptance proof implements it over the packed NAPI
// addon and fails on any TypeScript fallback):
//   adapter.state {} -> { availability, authorization, power,
//     backendGeneration, updatedAt, safeReason }
//   scan.start { serviceUuids: string[], timeoutMs: number | null }
//     -> { operationId: string }
//   scan.take {} -> core observation record | null (null = none queued now)
//   scan.stop { operationId: string } -> { state: 'released' | ... }
//   connection.connect { peerId: string, lease: string,
//     timeoutMs: number | null } -> { peerKey: string,
//     connectionGeneration: string }
//   connection.disconnect { peerId: string, lease: string } -> {}
//   gatt.discover { peerId: string, lease: string }
//     -> { services: [{ uuid, occurrence, characteristics:
//        [{ uuid, occurrence, properties, descriptors:
//        [{ uuid, occurrence }] }] }] }
//   gatt.read { peerId: string, selector, timeoutMs: number | null }
//     -> { value: bytes }
//   gatt.write { peerId: string, selector, value: bytes,
//     mode: 'with-response' | 'without-response',
//     timeoutMs: number | null } -> {}
//   gatt.subscribe { peerId: string, selector, consumer: string,
//     timeoutMs: number | null } -> {}
//   notifications.take { peerId: string, selector, consumer: string }
//     -> { value: bytes } | null
//   gatt.unsubscribe { peerId: string, selector, consumer: string }
//     -> { disabled: boolean }
//   peers.resolve { reference: PeerReference }
//     -> backend peer record | null
//   peers.known { services?: string[] } -> backend peer records
//   peers.connected {} -> backend peer records
//   events.take {} -> BackendEvent record | null
//   counters.describe {} -> ResourceCounters record
//   op.cancel { operationId: string }
//     -> { state: 'cancellation-requested' | 'already-terminal' | 'not-cancellable' }
//   session.dispose {} -> { state: string } (real destroy transition,
//     idempotent; the provider then closes the session)
//
// Bytes cross as Uint8Array in process; out-of-process bindings may use
// `{ base64: string }` and the provider decodes both. Failures thrown by the
// session (frozen `code|domain|operation` identities) propagate verbatim:
// the provider never substitutes a TypeScript error identity for a core one.

import type {
  AdapterBackend,
  BackendAttachment,
  BackendAttachmentRequest,
  BackendEvent,
  BackendPeerQuery,
  BackendPeerRecord,
  BackendSubscription,
  BleCentralBackend,
  ConnectionBackend,
  ConnectionLease,
  ConnectionOptions,
  GattBackend,
  PeerAddressDescriptor,
  PeerDirectoryBackend,
  ResourceCounters,
  ScanLease,
  ScannerBackend
} from '../../backend-contract/backend'
import type {
  AdapterStateSnapshot,
  AdapterStateWatch,
  AttachmentRecord,
  NativeBackendIdentity
} from '../../backend-contract/identity'
import type { AdvertisementObservation, OwnerScanOptions } from '../../backend-contract/advertisement'
import {
  createAttachmentBoundIdFactory,
  canonicalUuid,
  capacity,
  negotiateCoreVersions,
  negotiateVersion,
  opaqueId,
  resourceCount,
  type AttachmentBoundIdFactory,
  type ClientId,
  type LeaseId,
  type MonotonicTimestamp,
  type NativeVersionAxes,
  type OwnedBytes,
  type PeerId,
  type ScanShareToken,
  type Uuid
} from '../../backend-contract/primitives'
import { createGattCharacteristicProperties } from '../../backend-contract/gatt'
import type { BackendConnection } from '../../backend-contract/backend'
import {
  createBackendOperationDispatch,
  type BackendOperationDispatch,
  type CancellationAcknowledgement,
  type OperationOptions,
  type OperationTerminalRecord,
  type PublicOperationOptions,
  type ReadRequest,
  type ReadResult,
  type SubscribeRequest,
  type WriteRequest,
  type WriteResult
} from '../../backend-contract/operations'
import type { CharacteristicPath, DescriptorPath, GattDatabase } from '../../backend-contract/gatt'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream } from '../../core/bounded-stream'
import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry,
  type BuiltInFeatureId
} from '../../backend-contract/capabilities'
import { contractError } from '../../backend-contract/errors'
import type { AdapterSelection } from '../../backend-contract/identity'
import type { PeerReference } from '../../backend-contract/peer-reference'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import type { Spec as NativeProtocolControl } from '../../NativeUnifiedBleProtocolControl'
import { createReactNativeConnectionControlFeatureRegistry } from './react-native-connection-control-features'
import { createReactNativeDescriptorFeatureRegistry } from './react-native-descriptor-features'
import {
  combineReactNativeFeatureRegistries,
  createReactNativeRestorationFeatureRegistry,
  ReactNativeRestorationCoordinator,
  type ReactNativeRestorationActivation,
  type ReactNativeRestorationBackendProvider
} from './react-native-restoration'
import {
  reactNativeAndroidCompatibility,
  reactNativeAndroidDefaultAdapterId,
  REACT_NATIVE_ANDROID_BACKEND_ID,
  REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_ANDROID_PLATFORM_ID
} from './react-native-android-provider'
import {
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId,
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID
} from './react-native-apple-provider'
import {
  dispatchReactNativeRustCoreOp,
  openAdmittedRustCoreSession,
  type ReactNativeRustCoreBinding,
  type ReactNativeRustCoreSession
} from './react-native-rust-core'

export type ReactNativeRustCorePlatform = 'android' | 'apple'

export const REACT_NATIVE_RUST_CORE_BACKEND_ID = 'unified-ble:react-native-rust-core'
export const REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

export interface ReactNativeRustCoreProviderOptions {
  /** Target mobile platform (selects the adapter identity and compatibility). */
  readonly platform: ReactNativeRustCorePlatform
  /**
   * The injected native Rust core binding. There is no default and no
   * TypeScript fallback: resolution/admission fail loudly through the seam.
   */
  readonly binding: ReactNativeRustCoreBinding
  /** Owner label for the admitted core session (host identity). */
  readonly owner: string
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Native restoration control for the coordinator (identity only, never BLE work). */
  readonly control: NativeProtocolControl
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
}

export interface ReactNativeRustCoreBackendProvider extends ReactNativeRestorationBackendProvider {
  create(selection: AdapterSelection<string>): Promise<ReactNativeRustCoreBackend>
}

let nextRustCoreOwner = 1

function allocateRustCoreOwnerId(): string {
  const ordinal = nextRustCoreOwner
  nextRustCoreOwner += 1
  return `react-native-rust-core-owner-${ordinal}`
}

function compatibilityFor(platform: ReactNativeRustCorePlatform) {
  return platform === 'android' ? reactNativeAndroidCompatibility : reactNativeAppleCompatibility
}

function backendIdFor(platform: ReactNativeRustCorePlatform): string {
  return platform === 'android' ? REACT_NATIVE_ANDROID_BACKEND_ID : REACT_NATIVE_APPLE_BACKEND_ID
}

function platformIdFor(platform: ReactNativeRustCorePlatform): string {
  return platform === 'android' ? REACT_NATIVE_ANDROID_PLATFORM_ID : REACT_NATIVE_APPLE_PLATFORM_ID
}

function adapterNativeIdFor(platform: ReactNativeRustCorePlatform): string {
  return platform === 'android'
    ? REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID
    : REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID
}

function defaultAdapterIdFor(platform: ReactNativeRustCorePlatform) {
  return platform === 'android' ? reactNativeAndroidDefaultAdapterId() : reactNativeAppleDefaultAdapterId()
}

/**
 * Creates the binding-backed provider. The native binding is injected by
 * the host application; without one the provider cannot be constructed and
 * the factory never silently substitutes the TypeScript manager.
 */
export function createReactNativeRustCoreBackendProvider(
  options: ReactNativeRustCoreProviderOptions
): ReactNativeRustCoreBackendProvider {
  if (options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-rust-core.provider.owner')
  }
  const createOwnerId = options.createOwnerId ?? allocateRustCoreOwnerId
  const restoration = new ReactNativeRestorationCoordinator(options.control, options.platform)
  const compatibility = compatibilityFor(options.platform)
  return Object.freeze({
    descriptor: Object.freeze({
      providerId: 'unified-ble:react-native-rust-core-provider',
      hostKind: 'native-mobile',
      loadability: 'loadable',
      compatibility
    }),
    restoration,
    listAdapters: async () => {
      const backend = await openRustCoreBackend(options, createOwnerId(), restoration, false)
      try {
        return Object.freeze([backend.identity.attachment.adapter])
      } finally {
        await backend.destroy()
      }
    },
    create: async (selection: AdapterSelection<string>) => {
      if (String(selection.selectedAdapterId) !== String(defaultAdapterIdFor(options.platform))) {
        throw contractError('adapter.unavailable', 'adapter', 'react-native-rust-core.provider.select-adapter')
      }
      return openRustCoreBackend(options, createOwnerId(), restoration, true)
    }
  })
}

async function openRustCoreBackend(
  options: ReactNativeRustCoreProviderOptions,
  sessionOwner: string,
  restoration: ReactNativeRestorationCoordinator,
  activateRestoration: boolean
): Promise<ReactNativeRustCoreBackend> {
  if (sessionOwner.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-rust-core.provider.owner-id')
  }
  const session = await openAdmittedRustCoreSession(options.binding, `${options.owner}/${sessionOwner}`)
  const backend = new ReactNativeRustCoreBackend(options.platform, session, options.now, restoration)
  try {
    await backend.open()
    if (activateRestoration) {
      backend.activateRestoration(restoration)
    }
    return backend
  } catch (error) {
    await backend.destroy().catch(() => undefined)
    throw error
  }
}

/** Core observation record wire shape (JSON from out-of-process bindings). */
interface RustCoreObservation {
  readonly peerId: string
  readonly rssi?: number | null
  readonly localName?: string | null
  readonly serviceUuids?: readonly string[]
  readonly manufacturerData?: ReadonlyArray<{ companyId: number; payload: unknown }>
  readonly serviceData?: ReadonlyArray<{ uuid: string; payload: unknown }>
  readonly txPower?: number | null
  readonly connectable?: boolean | null
  readonly sourceTimestampMs?: number | null
  readonly ingressOrdinal?: number | null
}

interface RustCorePeerRecord {
  readonly peerId: string
  readonly name: string | null
  readonly rssi: number | null
  readonly source: string
  readonly reachability: string
  readonly connection: string
  readonly bond: string
  readonly lastSeenAtMonotonicMs: number | null
}

interface RustCoreDatabase {
  readonly services: ReadonlyArray<{
    readonly uuid: string
    readonly occurrence: number
    readonly characteristics: ReadonlyArray<{
      readonly uuid: string
      readonly occurrence: number
      readonly properties: number
      readonly descriptors: ReadonlyArray<{ readonly uuid: string; readonly occurrence: number }>
    }>
  }>
}

/**
 * Binding-backed backend: every BLE data-path method dispatches through the
 * admitted session. No timers, no retries, no subscription bookkeeping live
 * here — the core owns admission, deadlines, overflow, and teardown.
 */
export class ReactNativeRustCoreBackend implements BleCentralBackend<string, NativeBackendIdentity<string>> {
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly peers: PeerDirectoryBackend<string>
  readonly features: ReturnType<typeof createReactNativeRustCoreFeatureRegistry>
  readonly security = undefined

  private identifiers: AttachmentBoundIdFactory<string>
  private attachment: AttachmentRecord<string>
  private readonly peerIdsByNativeId = new Map<string, PeerId<string>>()
  private readonly nativeIdsByPeerId = new Map<string, string>()
  private nextPeer = 1
  private nextScan = 1
  private nextConnection = 1
  private nextLease = 1
  private nextOperation = 1
  private destroyed = false
  private destroyResult: Promise<import('../../backend-contract/errors').CleanupRecord> | null = null
  private restorationActivation: ReactNativeRestorationActivation | null = null
  private readonly eventsStream: CoreBoundedStream<BackendEvent<string>>
  private eventsPumpStarted = false
  private eventsStopped = false

  constructor(
    private readonly platform: ReactNativeRustCorePlatform,
    private readonly session: ReactNativeRustCoreSession,
    private readonly now: () => number,
    private readonly restoration: ReactNativeRestorationCoordinator
  ) {
    const attachmentId = opaqueId(`rust-core-attachment-${platform}`, 'attachment', 'react-native-rust-core')
    const backendInstanceId = opaqueId(
      `react-native-rust-core-backend-${platform}`,
      'backend-instance',
      'react-native-rust-core'
    )
    const backendGeneration = opaqueId(
      `rust-core-backend-generation-${platform}`,
      'backend-generation',
      'react-native-rust-core'
    )
    const adapterId = opaqueId(adapterNativeIdFor(platform), 'adapter', 'react-native-rust-core')
    const adapterGeneration = opaqueId(
      `rust-core-adapter-generation-${platform}`,
      'adapter-generation',
      'react-native-rust-core'
    )
    // The attachment tuple is frozen at open: identity equality covers the
    // adapter state (including updatedAt), so live radio state must never
    // rewrite it. Live state is served by adapter.currentState().
    this.attachment = Object.freeze({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapter: Object.freeze({
        adapterId,
        displayName: platform === 'android' ? 'Android default BLE adapter' : 'Apple default BLE adapter',
        state: Object.freeze({
          availability: 'unknown',
          authorization: 'unknown',
          power: 'unknown',
          backendGeneration,
          updatedAt: 0 as MonotonicTimestamp,
          safeReason: 'rust-core attachment state loads on open'
        }),
        adapterGeneration,
        limitations: Object.freeze([
          'The native Rust core owns radio scheduling; this backend carries no TypeScript radio policy'
        ])
      })
    })
    this.identifiers = createAttachmentBoundIdFactory<string>({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapterId,
      adapterGeneration
    })
    this.features = createReactNativeRustCoreFeatureRegistry(platform)
    this.eventsStream = new CoreBoundedStream<BackendEvent<string>>(
      { itemCapacity: capacity(256), byteCapacity: capacity(262144), reservedControlCapacity: capacity(1024) },
      'drop-oldest'
    )
    this.adapter = Object.freeze({
      currentState: () => this.currentAdapterState(),
      watchState: async () => this.watchAdapterState()
    })
    this.scanner = Object.freeze({
      start: (options: OwnerScanOptions<string, string>, clientId: ClientId<string, string>) =>
        this.startScan(options, clientId),
      join: (
        _sharedLeaseId: LeaseId<string, string>,
        _shareToken: ScanShareToken<string, string>,
        _clientId: ClientId<string, string>
      ): Promise<ScanLease<string, string>> => {
        throw contractError('capability.unsupported', 'scan', 'react-native-rust-core.scan.join')
      }
    })
    this.connections = Object.freeze({
      connect: (peerId: PeerId<string>, clientId: ClientId<string, string>, options: ConnectionOptions) =>
        this.connect(peerId, clientId, options),
      peerFromAddress: (descriptor: PeerAddressDescriptor) => this.peerFromAddress(descriptor)
    })
    this.gatt = Object.freeze({
      discover: (connection: BackendConnection<string, string>, options: PublicOperationOptions) =>
        this.discover(connection, options),
      read: (
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: ReadRequest<string, string>
      ) => this.read(path, request),
      write: (
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: WriteRequest<string, string>
      ) => this.write(path, request),
      readDescriptor: (
        path: DescriptorPath<string, string, string, string, string, string, 'current'>,
        request: ReadRequest<string, string>
      ) => this.readDescriptor(path, request),
      writeDescriptor: (
        path: DescriptorPath<string, string, string, string, string, string, 'current'>,
        request: WriteRequest<string, string>
      ) => this.writeDescriptor(path, request),
      subscribe: (
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: SubscribeRequest<string, string>
      ) => this.subscribe(path, request),
      unsubscribe: (
        subscription: BackendSubscription<string, string, string, string, string>,
        operation: OperationOptions<string, string>
      ) => this.unsubscribe(subscription, operation)
    })
    this.peers = Object.freeze({
      resolve: (reference: PeerReference, options: BackendPeerQuery) => this.resolvePeer(reference, options),
      known: (options: BackendPeerQuery) => this.knownPeers(options),
      connected: (options: BackendPeerQuery) => this.connectedPeers(options),
      bonded: async (_options: BackendPeerQuery) => Object.freeze([]),
      authorized: async (_options: BackendPeerQuery) => Object.freeze([]),
      restored: async (_options: BackendPeerQuery) => Object.freeze([])
    })
  }

  /** Loads the frozen attachment adapter state from the core (open path). */
  async open(): Promise<void> {
    const state = await this.invokeRecord('adapter.state', {})
    this.attachment = Object.freeze({
      ...this.attachment,
      adapter: Object.freeze({
        ...this.attachment.adapter,
        state: Object.freeze(this.parseAdapterState(state))
      })
    })
    await this.refreshCountersStrict()
    this.ensureEventsPump()
  }

  activateRestoration(restoration: ReactNativeRestorationCoordinator): void {
    this.restorationActivation = restoration.activate(this.attachment, this.nativeVersions() as NativeVersionAxes)
  }

  private nativeVersions(): NativeVersionAxes {
    const compatibility = compatibilityFor(this.platform)
    return Object.freeze({
      ...negotiateCoreVersions(compatibility, compatibility),
      nativeProtocol: negotiateVersion(compatibility.nativeProtocol, compatibility.nativeProtocol)
    })
  }

  get identity(): NativeBackendIdentity<string> {
    return Object.freeze({
      registeredBackendId: backendIdFor(this.platform),
      registeredPlatformId: platformIdFor(this.platform),
      attachment: this.attachment,
      versions: this.nativeVersions(),
      runtime: Object.freeze({
        hostKind: 'native-mobile',
        implementationVersion: REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          boundary: 'react-native-rust-core-v1',
          transport: 'native-core-session'
        })
      })
    })
  }

  async attach(_request: BackendAttachmentRequest): Promise<BackendAttachment<string, NativeBackendIdentity<string>>> {
    this.assertOperational('react-native-rust-core.attach')
    return Object.freeze({ attachment: this.attachment, identity: this.identity })
  }

  events(): BoundedAsyncStream<BackendEvent<string>> {
    return this.eventsStream
  }

  resourceCounters(): ResourceCounters {
    // Counters are core-owned; serve the last-known snapshot without
    // blocking the caller, and refresh it on every read. A core that cannot
    // report fails loudly rather than serving zeros as healthy data.
    const snapshot = this.lastCounters
    if (snapshot === null) {
      throw contractError('lifecycle.invariant-violation', 'core', 'react-native-rust-core.counters-unavailable')
    }
    this.refreshCounters().catch(() => undefined)
    return snapshot
  }

  destroy(): Promise<import('../../backend-contract/errors').CleanupRecord> {
    if (this.destroyResult === null) {
      const destruction = this.destroyInternal()
      this.destroyResult = destruction.then(
        cleanup => {
          if (cleanup.state !== 'released' || cleanup.failures.length !== 0) {
            this.destroyResult = null
          }
          return cleanup
        },
        error => {
          this.destroyResult = null
          throw error
        }
      )
    }
    return this.destroyResult
  }

  private lastCounters: ResourceCounters | null = null

  private async refreshCountersStrict(): Promise<void> {
    const record = await this.invokeRecord('counters.describe', {})
    this.lastCounters = parseResourceCounters(record)
  }

  private async refreshCounters(): Promise<void> {
    try {
      const record = await this.invokeRecord('counters.describe', {})
      this.lastCounters = parseResourceCounters(record)
    } catch {
      // Keep serving the last-known snapshot; the ops themselves fail
      // loudly when the core is gone.
    }
  }

  private async destroyInternal(): Promise<import('../../backend-contract/errors').CleanupRecord> {
    this.destroyed = true
    this.eventsStopped = true
    try {
      if (this.restorationActivation !== null) {
        await this.restoration.deactivate(this.restorationActivation)
        this.restorationActivation = null
      }
    } finally {
      try {
        await dispatchReactNativeRustCoreOp(this.session, 'session.dispose', {})
      } finally {
        await this.session.close()
        await this.eventsStream.close()
      }
    }
    return { state: 'released', failures: [] }
  }

  private assertOperational(operation: string): void {
    if (this.destroyed) {
      throw contractError('lifecycle.destroyed', 'core', operation)
    }
  }

  private async invokeRecord(op: string, args: Record<string, unknown>): Promise<Record<string, unknown>> {
    const result = await dispatchReactNativeRustCoreOp(this.session, op, args)
    if (typeof result !== 'object' || result === null || Array.isArray(result)) {
      throw contractError('protocol.malformed', 'core', `react-native-rust-core.${op}.shape`)
    }
    return result as Record<string, unknown>
  }

  private timeoutMs(options: PublicOperationOptions): number | null {
    if (options.deadline === null || options.deadline === undefined) return null
    return Math.max(0, Number(options.deadline) - this.now())
  }

  private async requestCancellation(correlation: string): Promise<CancellationAcknowledgement<string>> {
    try {
      const record = await this.invokeRecord('op.cancel', { operationId: correlation })
      const state = record.state
      if (state === 'cancellation-requested' || state === 'already-terminal' || state === 'not-cancellable') {
        return { handle: this.identifiers.backendOperationHandle(correlation), state }
      }
    } catch {
      // A core that cannot report cancellation leaves the caller with the
      // core-owned outcome; report not-cancellable rather than inventing one.
    }
    return { handle: this.identifiers.backendOperationHandle(correlation), state: 'not-cancellable' }
  }

  private watchAbort(signal: AbortSignal | null, onAbort: () => void): void {
    if (signal === null) return
    if (signal.aborted) {
      onAbort()
      return
    }
    signal.addEventListener('abort', onAbort, { once: true })
  }

  // -- adapter -----------------------------------------------------------

  private parseAdapterState(record: Record<string, unknown>): AdapterStateSnapshot<string> {
    for (const field of ['availability', 'authorization', 'power'] as const) {
      if (typeof record[field] !== 'string') {
        throw contractError('protocol.malformed', 'core', `react-native-rust-core.adapter-state.${field}`)
      }
    }
    if (typeof record.backendGeneration !== 'string' || typeof record.updatedAt !== 'number') {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.adapter-state.generation')
    }
    if (record.safeReason !== null && record.safeReason !== undefined && typeof record.safeReason !== 'string') {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.adapter-state.reason')
    }
    return Object.freeze({
      availability: record.availability as AdapterStateSnapshot<string>['availability'],
      authorization: record.authorization as AdapterStateSnapshot<string>['authorization'],
      power: record.power as AdapterStateSnapshot<string>['power'],
      backendGeneration: this.attachment.adapter.state.backendGeneration,
      updatedAt: record.updatedAt as AdapterStateSnapshot<string>['updatedAt'],
      safeReason: (record.safeReason as string | null | undefined) ?? null
    })
  }

  private async currentAdapterState(): Promise<AdapterStateSnapshot<string>> {
    this.assertOperational('react-native-rust-core.adapter.state')
    return this.parseAdapterState(await this.invokeRecord('adapter.state', {}))
  }

  private async watchAdapterState(): Promise<AdapterStateWatch<string>> {
    const initial = await this.currentAdapterState()
    const transitions = new CoreBoundedStream<AdapterStateSnapshot<string>>(
      { itemCapacity: capacity(16), byteCapacity: capacity(4096), reservedControlCapacity: capacity(512) },
      'drop-oldest'
    )
    const watcher = (state: AdapterStateSnapshot<string>): void => {
      transitions.emit(state, 64)
    }
    this.adapterWatchers.add(watcher)
    return Object.freeze({ initial, transitions })
  }

  private readonly adapterWatchers = new Set<(state: AdapterStateSnapshot<string>) => void>()

  // -- peers -------------------------------------------------------------

  private peerIdForNativeId(nativePeerId: string): PeerId<string> {
    const existing = this.peerIdsByNativeId.get(nativePeerId)
    if (existing !== undefined) return existing
    const peerId = opaqueId(`rust-core-peer-${this.nextPeer}`, 'peer', 'react-native-rust-core')
    this.nextPeer += 1
    this.peerIdsByNativeId.set(nativePeerId, peerId)
    this.nativeIdsByPeerId.set(String(peerId), nativePeerId)
    return peerId
  }

  private nativeIdForPeerId(peerId: PeerId<string>, operation: string): string {
    const native = this.nativeIdsByPeerId.get(String(peerId))
    if (native === undefined) {
      throw contractError('peer.not-found', 'connection', operation)
    }
    return native
  }

  /**
   * Mints a connectable peer handle for a canonical radio address known out
   * of band. The address (lowercased) is the native id; the core resolves it
   * on connect, exactly as the reference backend canonicalizes addresses
   * before mapping them.
   */
  private peerFromAddress(descriptor: { address: string }): PeerId<string> {
    if (typeof descriptor?.address !== 'string' || descriptor.address.length === 0) {
      throw contractError('argument.invalid', 'connection', 'react-native-rust-core.peer-from-address')
    }
    return this.peerIdForNativeId(descriptor.address.toLowerCase())
  }

  private mapPeerRecord(record: RustCorePeerRecord): BackendPeerRecord<string> {
    return Object.freeze({
      reference: Object.freeze({
        version: 1,
        backendId: backendIdFor(this.platform),
        scope: 'origin',
        opaqueId: record.peerId
      }),
      peerId: this.peerIdForNativeId(record.peerId),
      name: record.name,
      rssi: record.rssi,
      source: record.source as BackendPeerRecord<string>['source'],
      state: Object.freeze({
        reachability: record.reachability as BackendPeerRecord<string>['state']['reachability'],
        connection: record.connection as BackendPeerRecord<string>['state']['connection'],
        bond: record.bond as BackendPeerRecord<string>['state']['bond'],
        lastSeenAtMonotonicMs: record.lastSeenAtMonotonicMs
      })
    })
  }

  private parsePeerRecord(value: unknown, operation: string): BackendPeerRecord<string> {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const record = value as Record<string, unknown>
    if (typeof record.peerId !== 'string') {
      throw contractError('protocol.malformed', 'core', operation)
    }
    return this.mapPeerRecord({
      peerId: record.peerId as string,
      name: typeof record.name === 'string' ? (record.name as string) : null,
      rssi: typeof record.rssi === 'number' ? (record.rssi as number) : null,
      source: typeof record.source === 'string' ? (record.source as string) : 'scan-observed',
      reachability: typeof record.reachability === 'string' ? (record.reachability as string) : 'unknown',
      connection: typeof record.connection === 'string' ? (record.connection as string) : 'unknown',
      bond: typeof record.bond === 'string' ? (record.bond as string) : 'unknown',
      lastSeenAtMonotonicMs:
        typeof record.lastSeenAtMonotonicMs === 'number' ? (record.lastSeenAtMonotonicMs as number) : null
    })
  }

  private async resolvePeer(
    reference: PeerReference,
    _options: BackendPeerQuery
  ): Promise<BackendPeerRecord<string> | null> {
    this.assertOperational('react-native-rust-core.peers.resolve')
    const result = await dispatchReactNativeRustCoreOp(this.session, 'peers.resolve', {
      reference: { ...reference }
    })
    if (result === null || result === undefined) return null
    return this.parsePeerRecord(result, 'react-native-rust-core.peers.resolve.shape')
  }

  private async knownPeers(_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> {
    this.assertOperational('react-native-rust-core.peers.known')
    const result = await dispatchReactNativeRustCoreOp(this.session, 'peers.known', {})
    if (!Array.isArray(result)) {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.peers.known.shape')
    }
    return Object.freeze(result.map(entry => this.parsePeerRecord(entry, 'react-native-rust-core.peers.known.shape')))
  }

  private async connectedPeers(_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> {
    this.assertOperational('react-native-rust-core.peers.connected')
    const result = await dispatchReactNativeRustCoreOp(this.session, 'peers.connected', {})
    if (!Array.isArray(result)) {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.peers.connected.shape')
    }
    return Object.freeze(
      result.map(entry => this.parsePeerRecord(entry, 'react-native-rust-core.peers.connected.shape'))
    )
  }

  // -- scan --------------------------------------------------------------

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    this.assertOperational('react-native-rust-core.scan.start')
    const serviceUuids = options.filter.serviceUuids.map(service => String(service))
    if (options.filter.manufacturerData.length > 0 || options.filter.localNamePrefix !== null) {
      throw contractError('capability.unsupported', 'scan', 'react-native-rust-core.scan.filter')
    }
    if (options.filter.deviceAddresses !== undefined && options.filter.deviceAddresses.length > 0) {
      throw contractError('capability.unsupported', 'scan', 'react-native-rust-core.scan.filter')
    }
    const ordinal = this.nextScan
    this.nextScan += 1
    const started = await this.invokeRecord('scan.start', {
      serviceUuids,
      timeoutMs: this.timeoutMs(options),
      duplicatePolicy: options.duplicatePolicy,
      timestampPolicy: options.timestampPolicy
    })
    if (typeof started.operationId !== 'string' || (started.operationId as string).length === 0) {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.scan.start.shape')
    }
    const operationId = started.operationId as string
    const scanSessionId = this.identifiers.scanSessionId(`rust-core-scan-session-${ordinal}`)
    const leaseId = this.identifiers.leaseId(`rust-core-scan-lease-${ordinal}`)
    const shareToken =
      options.sharing.mode === 'owner' && options.sharing.allowSharing
        ? this.identifiers.scanShareToken(`rust-core-scan-share-${ordinal}`)
        : null
    const observations = new CoreBoundedStream<AdvertisementObservation<string>>(
      options.delivery,
      options.delivery.overflowPolicy
    )
    let stopped = false
    const stop = async (): Promise<import('../../backend-contract/errors').CleanupRecord> => {
      if (stopped) return { state: 'released', failures: [] }
      stopped = true
      try {
        await dispatchReactNativeRustCoreOp(this.session, 'scan.stop', { operationId })
      } finally {
        await observations.close()
      }
      return { state: 'released', failures: [] }
    }
    this.watchAbort(options.signal, () => {
      stop().catch(() => undefined)
    })
    this.pumpScanObservations(observations, scanSessionId, () => stopped, stop).catch(() => undefined)
    return Object.freeze({ scanSessionId, leaseId, shareToken, observations, stop })
  }

  private async pumpScanObservations(
    observations: CoreBoundedStream<AdvertisementObservation<string>>,
    scanSessionId: import('../../backend-contract/primitives').ScanSessionId<string, string>,
    isStopped: () => boolean,
    stop: () => Promise<unknown>
  ): Promise<void> {
    try {
      for (;;) {
        if (isStopped()) return
        const next = await dispatchReactNativeRustCoreOp(this.session, 'scan.take', {})
        if (next === null || next === undefined) {
          await pumpDelay()
          continue
        }
        observations.emit(this.mapObservation(next, scanSessionId), 512)
      }
    } catch {
      if (!isStopped()) {
        await stop().catch(() => undefined)
      }
    }
  }

  private mapObservation(
    value: unknown,
    scanSessionId: import('../../backend-contract/primitives').ScanSessionId<string, string>
  ): AdvertisementObservation<string> {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.scan.observation')
    }
    const record = value as RustCoreObservation & Record<string, unknown>
    if (typeof record.peerId !== 'string' || record.peerId.length === 0) {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.scan.observation.peer')
    }
    const peerId = this.peerIdForNativeId(record.peerId)
    return Object.freeze({
      device: Object.freeze({
        id: peerId,
        backendInstanceId: this.attachment.backendInstanceId,
        scope: 'session',
        stableAcrossRestarts: false,
        address: null
      }),
      provenance: 'platform-raw',
      sourceTimestamp: presentField(
        typeof record.sourceTimestampMs === 'number'
          ? Object.freeze({
              monotonicMs: record.sourceTimestampMs as MonotonicTimestamp,
              origin: 'platform' as const
            })
          : null
      ),
      receivedAtMonotonicMs: this.now() as AdvertisementObservation<string>['receivedAtMonotonicMs'],
      ingressOrdinal:
        typeof record.ingressOrdinal === 'number'
          ? record.ingressOrdinal
          : (0 as AdvertisementObservation<string>['ingressOrdinal']),
      scanSessionId,
      localName: presentField<string>(typeof record.localName === 'string' ? record.localName : null),
      rssi: presentField<number>(typeof record.rssi === 'number' ? record.rssi : null),
      txPower: presentField<number>(typeof record.txPower === 'number' ? record.txPower : null),
      connectable: presentField<boolean>(typeof record.connectable === 'boolean' ? record.connectable : null),
      appearance: absentField<number>('appearance not reported by the core observation'),
      serviceUuids: presentField<readonly Uuid[]>(
        Array.isArray(record.serviceUuids)
          ? Object.freeze(
              (record.serviceUuids as unknown[]).map(entry =>
                uuidFromCore(String(entry), 'react-native-rust-core.scan.service-uuids')
              )
            )
          : null
      ),
      solicitedServiceUuids: absentField<readonly import('../../backend-contract/primitives').Uuid[]>(
        'solicited services not reported by the core observation'
      ),
      overflowServiceUuids: absentField<readonly import('../../backend-contract/primitives').Uuid[]>(
        'overflow services not reported by the core observation'
      ),
      serviceData: presentField(
        Array.isArray(record.serviceData)
          ? Object.freeze(
              (record.serviceData as Array<{ uuid: string; payload: unknown }>).map(entry =>
                Object.freeze({
                  serviceUuid: uuidFromCore(String(entry.uuid), 'react-native-rust-core.scan.service-data'),
                  value: ownedBytes(bytesFromCore(entry.payload))
                })
              )
            )
          : null
      ),
      manufacturerData: presentField(
        Array.isArray(record.manufacturerData)
          ? Object.freeze(
              (record.manufacturerData as Array<{ companyId: number; payload: unknown }>).map(entry =>
                Object.freeze({
                  companyIdentifier: Number(entry.companyId),
                  value: ownedBytes(bytesFromCore(entry.payload))
                })
              )
            )
          : null
      ),
      rawRecord: absentField<import('../../backend-contract/primitives').OwnedBytes>(
        'raw record not reported by the core observation'
      ),
      scanResponseRecord: absentField<import('../../backend-contract/primitives').OwnedBytes>(
        'scan response record not reported by the core observation'
      )
    })
  }

  // -- connections --------------------------------------------------------

  private readonly connectionLeases = new Map<string, { lease: string; nativePeerId: string }>()

  private async connect(
    peerId: PeerId<string>,
    _clientId: ClientId<string, string>,
    options: ConnectionOptions
  ): Promise<ConnectionLease<string, string, string>> {
    this.assertOperational('react-native-rust-core.connection.connect')
    const nativePeerId = this.nativeIdForPeerId(peerId, 'react-native-rust-core.connection.unknown-peer')
    const ordinal = this.nextConnection
    this.nextConnection += 1
    const leaseOrdinal = this.nextLease
    this.nextLease += 1
    const lease = `rust-core-lease-${leaseOrdinal}`
    const connected = await this.invokeRecord('connection.connect', {
      peerId: nativePeerId,
      lease,
      timeoutMs: this.timeoutMs(options),
      intent: options.intent ?? 'direct',
      transport: options.transport ?? 'auto',
      preferredPhy: options.preferredPhy ?? []
    })
    if (typeof connected.peerKey !== 'string' || typeof connected.connectionGeneration !== 'string') {
      throw contractError('protocol.malformed', 'core', 'react-native-rust-core.connection.connect.shape')
    }
    const connectionId = this.identifiers.connectionId(`rust-core-connection-${ordinal}`)
    const leaseId = this.identifiers.leaseId(`rust-core-connection-lease-${leaseOrdinal}`)
    const connectionGeneration = opaqueId(
      String(connected.connectionGeneration),
      'connection-generation',
      'react-native-rust-core'
    )
    // The core matches discover/disconnect against the exact lease string
    // connect established: retain the raw core lease (not the branded
    // public leaseId) so every op on this connection addresses one lease.
    this.connectionLeases.set(String(connectionId), { lease, nativePeerId })
    const connection: BackendConnection<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      connectionGeneration,
      state: 'connected',
      disconnect: async () => this.disconnectConnection(nativePeerId, lease)
    })
    return Object.freeze({
      leaseId,
      connection,
      release: async () => this.disconnectConnection(nativePeerId, lease)
    })
  }

  private async disconnectConnection(
    nativePeerId: string,
    lease: string
  ): Promise<import('../../backend-contract/errors').CleanupRecord> {
    await dispatchReactNativeRustCoreOp(this.session, 'connection.disconnect', { peerId: nativePeerId, lease })
    return { state: 'released', failures: [] }
  }

  // -- GATT ----------------------------------------------------------------

  private selectorFor(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): Record<string, unknown> {
    const service = path as unknown as Record<string, unknown>
    for (const field of ['serviceUuid', 'characteristicUuid'] as const) {
      if (typeof service[field] !== 'string') {
        throw contractError('protocol.malformed', 'core', `${operation}.selector`)
      }
    }
    return {
      serviceUuid: service.serviceUuid as string,
      serviceOccurrence: this.occurrenceNumeral(String(service.serviceOccurrence), `${operation}.service-occurrence`),
      characteristicUuid: service.characteristicUuid as string,
      characteristicOccurrence: this.occurrenceNumeral(
        String(service.characteristicOccurrence),
        `${operation}.characteristic-occurrence`
      )
    }
  }

  private descriptorSelectorFor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): Record<string, unknown> {
    const selector = this.selectorFor(path, operation)
    const record = path as unknown as Record<string, unknown>
    if (typeof record.descriptorUuid !== 'string') {
      throw contractError('protocol.malformed', 'core', `${operation}.selector`)
    }
    return {
      ...selector,
      descriptorUuid: record.descriptorUuid as string,
      descriptorOccurrence: this.occurrenceNumeral(
        String(record.descriptorOccurrence),
        `${operation}.descriptor-occurrence`
      )
    }
  }

  private nativePeerForPath(path: { peerId?: unknown }, operation: string): string {
    const peerId = (path as { peerId?: unknown }).peerId
    // Characteristic paths carry the connection-scoped peer through their
    // connection path; resolve the native id from the mapped opaque peer.
    if (typeof peerId === 'string') {
      const native = this.nativeIdsByPeerId.get(peerId)
      if (native !== undefined) return native
    }
    // Otherwise search connection records: paths built by this backend
    // always reference a mapped peer.
    throw contractError('peer.not-found', 'connection', operation)
  }

  private succeededTerminal(
    correlation: OperationTerminalRecord<string, string>['correlation']
  ): OperationTerminalRecord<string, string> {
    return Object.freeze({ correlation, outcome: 'succeeded', cause: null })
  }

  private readonly databases = new Map<string, StoredRustCoreDatabase>()
  private readonly occurrenceNumerals = new Map<string, number>()

  private mintOccurrence(kind: string, numeral: number): string {
    // Occurrence identities are decimal strings of the core numeral (the
    // portable snapshot layer requires `/^(0|[1-9][0-9]*)$/`): the brand
    // carries scope, the value stays the numeral.
    const id = String(opaqueId(String(numeral), kind, 'react-native-rust-core'))
    this.occurrenceNumerals.set(id, numeral)
    return id
  }

  private occurrenceNumeral(id: string, operation: string): number {
    const numeral = this.occurrenceNumerals.get(id)
    if (numeral === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    return numeral
  }

  private async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    this.assertOperational('react-native-rust-core.gatt.discover')
    const nativePeerId = this.nativeIdForPeerId(connection.peerId, 'react-native-rust-core.gatt.discover.peer')
    const lease = this.connectionLeases.get(String(connection.connectionId))?.lease ?? ''
    const report = await this.invokeRecord('gatt.discover', {
      peerId: nativePeerId,
      lease,
      timeoutMs: this.timeoutMs(options)
    })
    const tree = parseDatabase(report, 'react-native-rust-core.gatt.discover.shape')
    const databaseOrdinal = this.nextOperation
    this.nextOperation += 1
    const databaseId = this.identifiers.databaseId(`rust-core-database-${databaseOrdinal}`)
    const databaseGeneration = opaqueId(
      `rust-core-database-generation-${databaseOrdinal}`,
      'database-generation',
      'react-native-rust-core'
    )
    const path = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId: connection.peerId,
      connectionId: connection.connectionId,
      ownerLeaseId: this.identifiers.leaseId(`rust-core-database-lease-${databaseOrdinal}`),
      connectionGeneration: connection.connectionGeneration,
      databaseId,
      databaseGeneration
    })
    // Mint stable occurrence identities once per discovery: snapshot paths
    // flow back into this.gatt.* unchanged, and the numerals map back to
    // the core selector occurrences for the wire.
    const stored: StoredRustCoreDatabase = { tree, base: path, services: [] }
    tree.services.forEach(service => {
      const serviceOccurrence = this.mintOccurrence('service-occurrence', service.occurrence)
      const characteristics = service.characteristics.map(characteristic => {
        const characteristicOccurrence = this.mintOccurrence('characteristic-occurrence', characteristic.occurrence)
        const descriptors = characteristic.descriptors.map(descriptor =>
          this.mintOccurrence('descriptor-occurrence', descriptor.occurrence)
        )
        return { characteristic, characteristicOccurrence, descriptors }
      })
      stored.services.push({ service, serviceOccurrence, characteristics })
    })
    this.databases.set(String(databaseId), stored)
    return Object.freeze({
      path,
      snapshot: async () => this.databaseSnapshot(path),
      read: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ) => this.databaseRead(path, characteristic, readOptions),
      write: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        value: import('../../backend-contract/primitives').BorrowedBytes,
        writeOptions: import('../../backend-contract/operations').WritePolicy
      ) => this.databaseWrite(path, characteristic, value, writeOptions),
      readDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ) => this.databaseReadDescriptor(path, descriptor, readOptions),
      writeDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        value: import('../../backend-contract/primitives').BorrowedBytes,
        writeOptions: import('../../backend-contract/operations').WritePolicy
      ) => this.databaseWriteDescriptor(path, descriptor, value, writeOptions),
      subscribe: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        subscribeOptions: import('../../backend-contract/operations').SubscriptionOptions
      ) => this.databaseSubscribe(path, characteristic, subscribeOptions)
    })
  }

  private storedDatabase(path: { databaseId: unknown }, operation: string): StoredRustCoreDatabase {
    const stored = this.databases.get(String(path.databaseId))
    if (stored === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    return stored
  }

  private async databaseSnapshot(
    path: GattDatabase<string, string, string>['path']
  ): Promise<import('../../backend-contract/gatt').GattDatabaseSnapshot<string, string, string>> {
    this.assertOperational('react-native-rust-core.gatt.snapshot')
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.snapshot')
    const services: import('../../backend-contract/gatt').Service<string, string, string, string>[] = []
    const characteristics: import('../../backend-contract/gatt').Characteristic<
      string,
      string,
      string,
      string,
      string
    >[] = []
    const descriptors: import('../../backend-contract/gatt').Descriptor<
      string,
      string,
      string,
      string,
      string,
      string
    >[] = []
    for (const entry of stored.services) {
      const servicePath = Object.freeze({
        ...stored.base,
        serviceUuid: uuidFromCore(entry.service.uuid, 'react-native-rust-core.gatt.snapshot.service'),
        serviceOccurrence: entry.serviceOccurrence as import('../../backend-contract/primitives').GenerationId<
          'service-occurrence',
          string
        >
      })
      services.push(Object.freeze({ path: servicePath, primary: true, includedServices: Object.freeze([]) }))
      for (const characteristicEntry of entry.characteristics) {
        const characteristicPath = Object.freeze({
          ...servicePath,
          characteristicUuid: uuidFromCore(
            characteristicEntry.characteristic.uuid,
            'react-native-rust-core.gatt.snapshot.characteristic'
          ),
          characteristicOccurrence:
            characteristicEntry.characteristicOccurrence as import('../../backend-contract/primitives').GenerationId<
              'characteristic-occurrence',
              string
            >,
          validity: 'current' as const
        })
        characteristics.push(
          Object.freeze({
            path: characteristicPath,
            properties: characteristicPropertiesFromBits(characteristicEntry.characteristic.properties),
            access: Object.freeze({ read: 'unknown', write: 'unknown' as const })
          })
        )
        characteristicEntry.characteristic.descriptors.forEach((descriptor, descriptorIndex) => {
          descriptors.push(
            Object.freeze({
              path: Object.freeze({
                ...characteristicPath,
                descriptorUuid: uuidFromCore(descriptor.uuid, 'react-native-rust-core.gatt.snapshot.descriptor'),
                descriptorOccurrence: characteristicEntry.descriptors[
                  descriptorIndex
                ] as import('../../backend-contract/primitives').GenerationId<'descriptor-occurrence', string>
              }),
              properties: Object.freeze({
                read: true,
                write: true,
                availability: Object.freeze({ read: 'unknown' as const, write: 'unknown' as const }),
                access: Object.freeze({ read: 'unknown' as const, write: 'unknown' as const })
              })
            })
          )
        })
      }
    }
    return Object.freeze({
      path: stored.base,
      services: Object.freeze(services),
      characteristics: Object.freeze(characteristics),
      descriptors: Object.freeze(descriptors)
    })
  }

  private resolveCharacteristic(
    stored: StoredRustCoreDatabase,
    characteristic: {
      serviceUuid?: unknown
      serviceOccurrence?: unknown
      characteristicUuid?: unknown
      characteristicOccurrence?: unknown
    },
    operation: string
  ): CharacteristicPath<string, string, string, string, string, 'current'> {
    for (const entry of stored.services) {
      if (entry.service.uuid !== String(characteristic.serviceUuid)) continue
      if (entry.serviceOccurrence !== String(characteristic.serviceOccurrence)) continue
      for (const characteristicEntry of entry.characteristics) {
        if (characteristicEntry.characteristic.uuid !== String(characteristic.characteristicUuid)) continue
        if (characteristicEntry.characteristicOccurrence !== String(characteristic.characteristicOccurrence)) {
          continue
        }
        const servicePath = {
          ...stored.base,
          serviceUuid: uuidFromCore(entry.service.uuid, `${operation}.service`),
          serviceOccurrence: entry.serviceOccurrence as import('../../backend-contract/primitives').GenerationId<
            'service-occurrence',
            string
          >
        }
        return Object.freeze({
          ...servicePath,
          characteristicUuid: uuidFromCore(characteristicEntry.characteristic.uuid, `${operation}.characteristic`),
          characteristicOccurrence:
            characteristicEntry.characteristicOccurrence as import('../../backend-contract/primitives').GenerationId<
              'characteristic-occurrence',
              string
            >,
          validity: 'current' as const
        })
      }
    }
    throw contractError('gatt.not-found', 'gatt', operation)
  }

  private mintedCorrelation(
    kind: string
  ): import('../../backend-contract/primitives').OperationCorrelation<string, string> {
    const ordinal = this.nextOperation
    this.nextOperation += 1
    return this.identifiers.operationCorrelation(`rust-core-${kind}-${ordinal}`)
  }

  private async databaseRead(
    path: GattDatabase<string, string, string>['path'],
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<import('../../backend-contract/primitives').OwnedBytes> {
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.database-read')
    const resolved = this.resolveCharacteristic(stored, characteristic, 'react-native-rust-core.gatt.database-read')
    const dispatch = this.read(resolved, {
      operation: { signal: options.signal, deadline: options.deadline, correlation: this.mintedCorrelation('gdb-read') }
    })
    return (await dispatch.completion).value
  }

  private async databaseWrite(
    path: GattDatabase<string, string, string>['path'],
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    value: import('../../backend-contract/primitives').BorrowedBytes,
    options: import('../../backend-contract/operations').WritePolicy
  ): Promise<import('../../backend-contract/operations').WriteReceipt<string, string>> {
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.database-write')
    const resolved = this.resolveCharacteristic(stored, characteristic, 'react-native-rust-core.gatt.database-write')
    const dispatch = this.write(resolved, {
      operation: {
        signal: options.signal,
        deadline: options.deadline,
        correlation: this.mintedCorrelation('gdb-write')
      },
      bytes: value,
      mode: options.mode
    })
    return dispatch.completion
  }

  private async databaseReadDescriptor(
    path: GattDatabase<string, string, string>['path'],
    descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<import('../../backend-contract/primitives').OwnedBytes> {
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.database-read-descriptor')
    const resolved = this.resolveCharacteristic(
      stored,
      descriptor,
      'react-native-rust-core.gatt.database-read-descriptor'
    )
    const full = Object.freeze({
      ...resolved,
      descriptorUuid: (descriptor as unknown as Record<string, unknown>)
        .descriptorUuid as import('../../backend-contract/primitives').Uuid,
      descriptorOccurrence: (descriptor as unknown as Record<string, unknown>)
        .descriptorOccurrence as import('../../backend-contract/primitives').GenerationId<
        'descriptor-occurrence',
        string
      >
    })
    const dispatch = this.readDescriptor(full, {
      operation: {
        signal: options.signal,
        deadline: options.deadline,
        correlation: this.mintedCorrelation('gdb-read-desc')
      }
    })
    return (await dispatch.completion).value
  }

  private async databaseWriteDescriptor(
    path: GattDatabase<string, string, string>['path'],
    descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
    value: import('../../backend-contract/primitives').BorrowedBytes,
    options: import('../../backend-contract/operations').WritePolicy
  ): Promise<import('../../backend-contract/operations').WriteReceipt<string, string>> {
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.database-write-descriptor')
    const resolved = this.resolveCharacteristic(
      stored,
      descriptor,
      'react-native-rust-core.gatt.database-write-descriptor'
    )
    const full = Object.freeze({
      ...resolved,
      descriptorUuid: (descriptor as unknown as Record<string, unknown>)
        .descriptorUuid as import('../../backend-contract/primitives').Uuid,
      descriptorOccurrence: (descriptor as unknown as Record<string, unknown>)
        .descriptorOccurrence as import('../../backend-contract/primitives').GenerationId<
        'descriptor-occurrence',
        string
      >
    })
    const dispatch = this.writeDescriptor(full, {
      operation: {
        signal: options.signal,
        deadline: options.deadline,
        correlation: this.mintedCorrelation('gdb-write-desc')
      },
      bytes: value,
      mode: options.mode
    })
    return dispatch.completion
  }

  private async databaseSubscribe(
    path: GattDatabase<string, string, string>['path'],
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: import('../../backend-contract/operations').SubscriptionOptions
  ): Promise<import('../../backend-contract/gatt').Subscription<string, string, string, string, string, string>> {
    const stored = this.storedDatabase(path, 'react-native-rust-core.gatt.database-subscribe')
    const resolved = this.resolveCharacteristic(
      stored,
      characteristic,
      'react-native-rust-core.gatt.database-subscribe'
    )
    const dispatch = this.subscribe(resolved, {
      operation: {
        signal: options.signal,
        deadline: options.deadline,
        correlation: this.mintedCorrelation('gdb-subscribe')
      },
      options
    })
    const backendSubscription = await dispatch.completion
    return Object.freeze({
      subscriptionId: backendSubscription.subscriptionId,
      path: backendSubscription.path,
      values: backendSubscription.notifications,
      remove: async () => {
        const removal = this.unsubscribe(backendSubscription, {
          signal: null,
          deadline: null,
          correlation: this.mintedCorrelation('gdb-unsubscribe')
        })
        await removal.completion
        return { state: 'released', failures: [] } as import('../../backend-contract/errors').CleanupRecord
      }
    })
  }

  private dispatchFor<Result>(
    correlationValue: string,
    completion: Promise<Result>
  ): BackendOperationDispatch<string, Result> {
    const handle = this.identifiers.backendOperationHandle(correlationValue)
    return createBackendOperationDispatch<string, Result>(handle, completion, () =>
      this.requestCancellation(correlationValue)
    )
  }

  private read(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.assertOperational('react-native-rust-core.gatt.read')
    const selector = this.selectorFor(path, 'react-native-rust-core.gatt.read')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'react-native-rust-core.gatt.read.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const completion = (async (): Promise<ReadResult<string, string>> => {
      const result = await this.invokeRecord('gatt.read', { peerId: nativePeerId, selector, timeoutMs })
      return Object.freeze({
        value: ownedBytes(bytesFromCore(result.value)),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  private write(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.assertOperational('react-native-rust-core.gatt.write')
    const selector = this.selectorFor(path, 'react-native-rust-core.gatt.write')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'react-native-rust-core.gatt.write.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const value = bytesToCore(request.bytes)
    const mode = request.mode
    const completion = (async (): Promise<WriteResult<string, string>> => {
      await this.invokeRecord('gatt.write', { peerId: nativePeerId, selector, value, mode, timeoutMs })
      return Object.freeze({
        terminal: this.succeededTerminal(request.operation.correlation),
        commitState: 'confirmed'
      })
    })()
    this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  private readDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.assertOperational('react-native-rust-core.gatt.read-descriptor')
    const selector = this.descriptorSelectorFor(path, 'react-native-rust-core.gatt.read-descriptor')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'react-native-rust-core.gatt.read-descriptor.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const completion = (async (): Promise<ReadResult<string, string>> => {
      const result = await this.invokeRecord('gatt.read-descriptor', { peerId: nativePeerId, selector, timeoutMs })
      return Object.freeze({
        value: ownedBytes(bytesFromCore(result.value)),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  private writeDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.assertOperational('react-native-rust-core.gatt.write-descriptor')
    const selector = this.descriptorSelectorFor(path, 'react-native-rust-core.gatt.write-descriptor')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'react-native-rust-core.gatt.write-descriptor.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const value = bytesToCore(request.bytes)
    const mode = request.mode
    const completion = (async (): Promise<WriteResult<string, string>> => {
      await this.invokeRecord('gatt.write-descriptor', { peerId: nativePeerId, selector, value, mode, timeoutMs })
      return Object.freeze({
        terminal: this.succeededTerminal(request.operation.correlation),
        commitState: 'confirmed'
      })
    })()
    this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  private readonly subscriptionConsumers = new Map<
    string,
    { nativePeerId: string; selector: Record<string, unknown>; consumer: string; closed: boolean }
  >()

  private subscribe(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, string>
  ): BackendOperationDispatch<string, BackendSubscription<string, string, string, string, string>> {
    this.assertOperational('react-native-rust-core.gatt.subscribe')
    const selector = this.selectorFor(path, 'react-native-rust-core.gatt.subscribe')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'react-native-rust-core.gatt.subscribe.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const consumerOrdinal = this.nextOperation
    this.nextOperation += 1
    const consumer = `rust-core-consumer-${consumerOrdinal}`
    const completion = (async (): Promise<BackendSubscription<string, string, string, string, string>> => {
      await this.invokeRecord('gatt.subscribe', {
        peerId: nativePeerId,
        selector,
        consumer,
        deliveryMode: request.options.deliveryMode ?? 'prefer-notification',
        timeoutMs
      })
      const subscriptionId = this.identifiers.subscriptionId(`rust-core-subscription-${consumerOrdinal}`)
      const notifications = new CoreBoundedStream<import('../../backend-contract/gatt').NotificationValue>(
        request.options.delivery,
        request.options.delivery.overflowPolicy
      )
      const pumpState = { nativePeerId, selector, consumer, closed: false }
      this.subscriptionConsumers.set(String(subscriptionId), pumpState)
      const pump = (async (): Promise<void> => {
        try {
          for (;;) {
            if (pumpState.closed || this.destroyed) return
            const next = await dispatchReactNativeRustCoreOp(this.session, 'notifications.take', {
              peerId: nativePeerId,
              selector,
              consumer
            })
            if (next === null || next === undefined) {
              await pumpDelay()
              continue
            }
            if (typeof next !== 'object' || next === null || Array.isArray(next)) {
              throw contractError('protocol.malformed', 'core', 'react-native-rust-core.notifications.shape')
            }
            notifications.emit(
              Object.freeze({
                value: ownedBytes(bytesFromCore((next as Record<string, unknown>).value)),
                indication: false
              }),
              512
            )
          }
        } catch {
          if (!pumpState.closed) {
            await notifications.close().catch(() => undefined)
          }
        }
      })()
      pump.catch(() => undefined)
      const subscription = Object.freeze({
        subscriptionId,
        path,
        terminal: this.succeededTerminal(request.operation.correlation),
        notifications
      })
      return subscription as BackendSubscription<string, string, string, string, string>
    })()
    this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  private unsubscribe(
    subscription: BackendSubscription<string, string, string, string, string>,
    operation: OperationOptions<string, string>
  ): BackendOperationDispatch<string, OperationTerminalRecord<string, string>> {
    this.assertOperational('react-native-rust-core.gatt.unsubscribe')
    const stored = this.subscriptionConsumers.get(String(subscription.subscriptionId))
    const correlation = String(operation.correlation)
    const completion = (async (): Promise<OperationTerminalRecord<string, string>> => {
      if (stored !== undefined) {
        stored.closed = true
        try {
          await this.invokeRecord('gatt.unsubscribe', {
            peerId: stored.nativePeerId,
            selector: stored.selector,
            consumer: stored.consumer
          })
        } finally {
          this.subscriptionConsumers.delete(String(subscription.subscriptionId))
        }
      }
      await subscription.notifications.close().catch(() => undefined)
      return this.succeededTerminal(operation.correlation)
    })()
    this.watchAbort(operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    return this.dispatchFor(correlation, completion)
  }

  // -- backend events --------------------------------------------------------

  private ensureEventsPump(): void {
    if (this.eventsPumpStarted) return
    this.eventsPumpStarted = true
    this.pumpBackendEvents().catch(() => undefined)
  }

  private async pumpBackendEvents(): Promise<void> {
    try {
      for (;;) {
        if (this.eventsStopped) return
        const next = await dispatchReactNativeRustCoreOp(this.session, 'events.take', {})
        if (next === null || next === undefined) {
          await pumpDelay()
          continue
        }
        this.emitBackendEvent(next)
      }
    } catch {
      if (!this.eventsStopped) {
        this.eventsStopped = true
      }
    }
  }

  private emitBackendEvent(value: unknown): void {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) return
    const record = value as Record<string, unknown>
    if (typeof record.kind !== 'string') return
    const kind = record.kind as string
    if (kind === 'adapter-state-changed') {
      try {
        const state = this.parseAdapterState((record.state as Record<string, unknown>) ?? {})
        for (const watcher of this.adapterWatchers) {
          try {
            watcher(state)
          } catch {
            // One slow watcher must not break event delivery.
          }
        }
        this.eventsStream.emit(
          {
            kind: 'adapter-state',
            attachment: this.attachment,
            attachmentId: this.attachment.attachmentId,
            ingressOrdinal: this.nextEventOrdinal()
          },
          64
        )
      } catch {
        // Malformed adapter events never break the pump.
      }
      return
    }
    // Lifecycle events the typed surface cannot express ride as
    // diagnostics; dropping them silently would hide core truth.
    this.eventsStream.emit(
      {
        kind: 'diagnostic',
        attachment: this.attachment,
        attachmentId: this.attachment.attachmentId,
        ingressOrdinal: this.nextEventOrdinal()
      },
      64
    )
  }

  private eventOrdinal = 1

  private nextEventOrdinal(): number {
    const ordinal = this.eventOrdinal
    this.eventOrdinal += 1
    return ordinal
  }
}

function numberField(record: Record<string, unknown>, field: string): number {
  const value = record[field]
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
    throw contractError('protocol.malformed', 'core', `react-native-rust-core.counters.${field}`)
  }
  return Math.floor(value)
}

interface StoredRustCoreService {
  readonly service: { readonly uuid: string; readonly occurrence: number }
  readonly serviceOccurrence: string
  readonly characteristics: ReadonlyArray<{
    readonly characteristic: {
      readonly uuid: string
      readonly occurrence: number
      readonly properties: number
      readonly descriptors: ReadonlyArray<{ readonly uuid: string; readonly occurrence: number }>
    }
    readonly characteristicOccurrence: string
    readonly descriptors: ReadonlyArray<string>
  }>
}

interface StoredRustCoreDatabase {
  readonly tree: RustCoreDatabase
  readonly base: import('../../backend-contract/gatt').DatabasePath<string, string, string>
  readonly services: StoredRustCoreService[]
}

function parseDatabase(value: Record<string, unknown>, operation: string): RustCoreDatabase {
  if (!Array.isArray(value.services)) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  const services = (value.services as unknown[]).map(entry => parseDatabaseService(entry, operation))
  return { services }
}

function parseDatabaseService(entry: unknown, operation: string): RustCoreDatabase['services'][number] {
  if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  const record = entry as Record<string, unknown>
  if (typeof record.uuid !== 'string' || typeof record.occurrence !== 'number') {
    throw contractError('protocol.malformed', 'core', operation)
  }
  if (!Array.isArray(record.characteristics)) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  return {
    uuid: record.uuid as string,
    occurrence: Math.floor(record.occurrence as number),
    characteristics: (record.characteristics as unknown[]).map(characteristic =>
      parseDatabaseCharacteristic(characteristic, operation)
    )
  }
}

function parseDatabaseCharacteristic(
  entry: unknown,
  operation: string
): RustCoreDatabase['services'][number]['characteristics'][number] {
  if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  const record = entry as Record<string, unknown>
  if (
    typeof record.uuid !== 'string' ||
    typeof record.occurrence !== 'number' ||
    typeof record.properties !== 'number'
  ) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  if (!Array.isArray(record.descriptors)) {
    throw contractError('protocol.malformed', 'core', operation)
  }
  return {
    uuid: record.uuid as string,
    occurrence: Math.floor(record.occurrence as number),
    properties: Math.floor(record.properties as number),
    descriptors: (record.descriptors as unknown[]).map(descriptor => {
      if (typeof descriptor !== 'object' || descriptor === null || Array.isArray(descriptor)) {
        throw contractError('protocol.malformed', 'core', operation)
      }
      const descriptorRecord = descriptor as Record<string, unknown>
      if (typeof descriptorRecord.uuid !== 'string' || typeof descriptorRecord.occurrence !== 'number') {
        throw contractError('protocol.malformed', 'core', operation)
      }
      return {
        uuid: descriptorRecord.uuid as string,
        occurrence: Math.floor(descriptorRecord.occurrence as number)
      }
    })
  }
}

function parseResourceCounters(
  record: Record<string, unknown>
): import('../../backend-contract/backend').ResourceCounters {
  return Object.freeze({
    activeScanControllers: resourceCount(numberField(record, 'activeScanControllers')),
    scanConsumers: resourceCount(numberField(record, 'scanConsumers')),
    chooserSessions: resourceCount(numberField(record, 'chooserSessions')),
    connectionLeases: resourceCount(numberField(record, 'connectionLeases')),
    physicalLinks: resourceCount(numberField(record, 'physicalLinks')),
    databaseSnapshots: resourceCount(numberField(record, 'databaseSnapshots')),
    physicalCccdEnablements: resourceCount(numberField(record, 'physicalCccdEnablements')),
    subscriptionConsumers: resourceCount(numberField(record, 'subscriptionConsumers')),
    queuedOperations: resourceCount(numberField(record, 'queuedOperations')),
    dispatchedOperations: resourceCount(numberField(record, 'dispatchedOperations')),
    retainedByteBuffers: resourceCount(numberField(record, 'retainedByteBuffers')),
    restorationRecords: resourceCount(numberField(record, 'restorationRecords')),
    orphanedIpcOwners: resourceCount(numberField(record, 'orphanedIpcOwners'))
  })
}

/** Accepts in-process bytes or `{ base64 }` from out-of-process bindings. */
function bytesFromCore(value: unknown): Uint8Array {
  if (value instanceof Uint8Array) return value
  if (typeof Buffer !== 'undefined' && Buffer.isBuffer(value)) return new Uint8Array(value)
  if (Array.isArray(value)) return Uint8Array.from(value as number[])
  if (typeof value === 'object' && value !== null && typeof (value as { base64?: unknown }).base64 === 'string') {
    const binary = Buffer.from((value as { base64: string }).base64, 'base64')
    return new Uint8Array(binary)
  }
  throw contractError('protocol.malformed', 'core', 'react-native-rust-core.bytes')
}

function bytesToCore(value: Uint8Array): Uint8Array {
  return Uint8Array.from(value)
}

function ownedBytes(value: Uint8Array): OwnedBytes {
  return new Uint8Array(value) as OwnedBytes
}

function uuidFromCore(value: unknown, operation: string): Uuid {
  if (typeof value !== 'string') {
    throw contractError('protocol.malformed', 'core', operation)
  }
  try {
    return canonicalUuid(value)
  } catch {
    throw contractError('protocol.malformed', 'core', operation)
  }
}

function presentField<Value>(
  value: Value | null
): import('../../backend-contract/advertisement').AdvertisementField<Value> {
  if (value === null || value === undefined) {
    return Object.freeze({
      state: 'absent',
      reason: 'not reported by the core observation',
      provenance: 'not-provided'
    }) as import('../../backend-contract/advertisement').AdvertisementField<Value>
  }
  return Object.freeze({
    state: 'present',
    value,
    provenance: 'observed'
  }) as import('../../backend-contract/advertisement').AdvertisementField<Value>
}

function absentField<Value>(reason: string): import('../../backend-contract/advertisement').AdvertisementField<Value> {
  return Object.freeze({
    state: 'absent',
    reason,
    provenance: 'not-provided'
  }) as import('../../backend-contract/advertisement').AdvertisementField<Value>
}

/**
 * Consumer-side delivery pacing between core `take` polls (mirrors the NAPI
 * proof's poll loop). This paces delivery only: admission, deadlines,
 * overflow, and teardown all stay core-owned.
 */
function pumpDelay(): Promise<void> {
  return new Promise<void>(resolve => {
    const timer = setTimeout(resolve, 5)
    // Delivery pacing must never hold the process (or a test runner) open:
    // unref where the runtime supports it (Node), no-op otherwise (Hermes).
    const unref = (timer as unknown as { unref?: () => void }).unref
    if (typeof unref === 'function') {
      unref.call(timer)
    }
  })
}

// Core property bits (ubm-core GATT_PROP_*): READ=0x01, WRITE=0x02,
// WRITE_NO_RESPONSE=0x04, NOTIFY=0x08, INDICATE=0x10.
function characteristicPropertiesFromBits(
  bits: number
): import('../../backend-contract/gatt').CharacteristicProperties {
  return createGattCharacteristicProperties({
    read: (bits & 0x01) !== 0,
    writeWithResponse: (bits & 0x02) !== 0,
    writeWithoutResponse: (bits & 0x04) !== 0,
    notify: (bits & 0x08) !== 0,
    indicate: (bits & 0x10) !== 0,
    broadcast: false,
    authenticatedSignedWrites: false,
    extendedProperties: false
  })
}

export function createReactNativeRustCoreFeatureRegistry(platform: ReactNativeRustCorePlatform) {
  return combineReactNativeFeatureRegistries(
    createReactNativeConnectionControlFeatureRegistry(platform, REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION),
    createReactNativeRustCoreScanPlatformFeatureRegistry(),
    createReactNativeRustCorePeerFeatureRegistry(),
    createReactNativeDescriptorFeatureRegistry(platform, REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION),
    createReactNativeRestorationFeatureRegistry(platform, REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION)
  )
}

function createReactNativeRustCoreScanPlatformFeatureRegistry() {
  return createFeatureRegistry(
    Object.freeze([
      createBackendOperationCapabilityRegistration({
        id: BUILT_IN_FEATURE_IDS.scanPlatformOptions,
        implementationVersion: REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION,
        sourceDigest: 'react-native-rust-core-scan-platform-options-v1',
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
        operation: 'scan:platform-options.invoke-without-scan'
      })
    ])
  )
}

function createReactNativeRustCorePeerFeatureRegistry() {
  const scenarioIds = ['capability.truth-limits-evidence-and-binding']
  const ids: readonly BuiltInFeatureId[] = Object.freeze([
    BUILT_IN_FEATURE_IDS.peerBonded,
    BUILT_IN_FEATURE_IDS.peerResolveReference,
    BUILT_IN_FEATURE_IDS.connectionDirect,
    BUILT_IN_FEATURE_IDS.connectionWhenAvailable
  ])
  return createFeatureRegistry(
    Object.freeze(
      ids.map(id =>
        createBackendOperationCapabilityRegistration({
          id,
          implementationVersion: REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION,
          sourceDigest: `react-native-rust-core-${id.replace(':', '-')}-v1`,
          tckSuiteId: 'capability.catalog-v2',
          requiredScenarioIds: scenarioIds,
          operation: `${id}.invoke-without-peer-directory`
        })
      )
    )
  )
}
