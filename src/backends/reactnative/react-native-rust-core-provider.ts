// src/backends/reactnative/react-native-rust-core-provider.ts
//
// The React Native backend over the process-owned Rust mobile owner
// (docs/MOBILE_RUST_WIRE.md). Every radio effect is a frozen wire op on one
// admitted session; every platform fact arrives as a drain record through the
// one wake-driven router (react-native-rust-core-drain.ts). This file keeps
// only what the backend contract needs on the JavaScript side: opaque public
// identities, the core-issued generations behind them, stream fan-out, and
// the retained ownership that makes cleanup retryable.
//
// Truth rules this file keeps:
// - generations are the core's (`adapter.state`, `connection.connect`,
//   `gatt.discover`, `link`, `db-changed`); a handle whose generation the core
//   retired fails before any native I/O (PR210-16);
// - a write reports the owner's receipt, a failed write the owner's commit
//   state; a notification reports the delivery the owner reported (PR210-13);
// - scan stop, unsubscribe, disconnect and dispose keep their native identity
//   until the owner confirms release, so a failed cleanup can be retried
//   (PR210-09, PR210-14);
// - retained bytes are charged at their actual size (PR210-17).

import type {
  AdapterBackend,
  BackendAttachment,
  BackendAttachmentRequest,
  BackendConnection,
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
import {
  advertisementMatchesFilter,
  assertScanFilter,
  type AdvertisementField,
  type AdvertisementObservation,
  type OwnerScanOptions,
  type ScanFilter,
  type SourceTimestamp
} from '../../backend-contract/advertisement'
import type {
  FeatureRegistry,
  MaximumWriteLengthFeatureInput,
  MaximumWriteLengthFeatureOutput
} from '../../backend-contract/capabilities'
import type {
  ConnectionMaximumWriteLengthMeasurement,
  ConnectionMaximumWriteLengthRequest,
  ConnectionPhyObservation,
  ConnectionPhyRequest,
  ConnectionPriorityRequest,
  EffectiveMtuMeasurement,
  EffectiveMtuRequest,
  MtuNegotiation,
  ReadPhyRequest,
  ReadRssiRequest,
  RequestMtuRequest,
  RequestPhyRequest,
  RequestPriorityRequest,
  RssiMeasurement
} from '../../backend-contract/connection-controls'
import {
  BackendContractError,
  contractError,
  type CleanupFailure,
  type CleanupRecord,
  type NormalizedBleError
} from '../../backend-contract/errors'
import {
  createGattCharacteristicProperties,
  createGattDescriptorProperties,
  type Characteristic,
  type CharacteristicPath,
  type CharacteristicProperties,
  type DatabasePath,
  type Descriptor,
  type DescriptorPath,
  type GattDatabase,
  type GattDatabaseSnapshot,
  type NotificationValue,
  type Service,
  type Subscription
} from '../../backend-contract/gatt'
import type {
  AdapterSelection,
  AdapterStateSnapshot,
  AdapterStateWatch,
  AttachmentRecord,
  NativeBackendIdentity
} from '../../backend-contract/identity'
import {
  createBackendOperationDispatch,
  type BackendOperationDispatch,
  type CancellationAcknowledgement,
  type OperationOptions,
  type OperationTerminalOutcome,
  type OperationTerminalRecord,
  type PublicOperationOptions,
  type ReadRequest,
  type CharacteristicRead,
  type CharacteristicReadResult,
  type ReadResult,
  type SubscribeRequest,
  type SubscriptionOptions,
  type WritePolicy,
  type WriteReceipt,
  type WriteRequest,
  type WriteResult
} from '../../backend-contract/operations'
import { assertPeerReference, encodePeerReference, type PeerReference } from '../../backend-contract/peer-reference'
import type { CoreTraceSink } from '../../core/trace-recorder'
import {
  canonicalBleAddress,
  canonicalUuid,
  capacity,
  createAttachmentBoundIdFactory,
  monotonicTimestamp,
  negotiateCoreVersions,
  negotiateVersion,
  opaqueId,
  resourceCount,
  type AttachmentBoundIdFactory,
  type BackendInstanceId,
  type BorrowedBytes,
  type ClientId,
  type GenerationId,
  type LeaseId,
  type NativeVersionAxes,
  type OperationCorrelation,
  type OwnedBytes,
  type PeerId,
  type ScanSessionId,
  type ScanShareToken,
  type SerializableRecord,
  type Uuid
} from '../../backend-contract/primitives'
import type { ScanPlan } from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import type { CoreStreamTerminalReason } from '../../core/bounded-stream'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { trustedServiceUuidFilter } from '../scan-planning/service-uuid-scan-planner'
import {
  reactNativeAndroidCompatibility,
  reactNativeAndroidDefaultAdapterId,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId,
  REACT_NATIVE_ANDROID_BACKEND_ID,
  REACT_NATIVE_ANDROID_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_ANDROID_PLATFORM_ID,
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID
} from './react-native-platform-identity'
import {
  resolveReactNativeRustCoreBinding,
  type ReactNativeRustCoreBinding,
  type ReactNativeRustCoreSession
} from './react-native-rust-core'
import { RustCoreDrainRouter } from './react-native-rust-core-drain'
import {
  createReactNativeRustCoreFeatureRegistry,
  type ReactNativeRustCoreRuntimeFacts
} from './react-native-rust-core-features'
import { RustCoreRestorationJournal, type ReactNativeRestorationAuthority } from './react-native-rust-core-restoration'
import { RustCoreSecurityBackend } from './react-native-rust-core-security'
import {
  ReactNativeRestorationCoordinator,
  type ReactNativeRestorationActivation,
  type ReactNativeRestorationBackendProvider
} from './react-native-restoration'
import {
  diagnosticReactNativeAndroidScanPlan,
  diagnosticReactNativeAppleScanPlan,
  planReactNativeAndroidScan,
  planReactNativeAppleScan
} from './react-native-scan-planner'
import {
  checkWriteReceipt,
  encodeBase64,
  remotePlatformDetail,
  type WireAdapterState,
  type WireCleanupRecord,
  type WireCounters,
  type WireDelivery,
  type WireDiscovery,
  type WireDrainRecord,
  type WireJsonObject,
  type WireOp,
  type WireOpResults,
  type WirePeerRecord,
  type WireResult,
  type WireRestoredPeer,
  type WireSecurityState
} from './rust-core-wire'

export type ReactNativeRustCorePlatform = 'android' | 'apple'

export const REACT_NATIVE_RUST_CORE_BACKEND_ID = 'unified-ble:react-native-rust-core'
export const REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

const SCOPE = 'react-native-rust-core'
/** The last `budgetMs` the owner admits (`ubm_core::contracts::MAX_TIMEOUT_MS`). */
const MAX_BUDGET_MS = 2147483647
/** Bytes a UUID occupies in a retained record (canonical text form). */
const UUID_BYTES = 36
/** Fixed bytes of one retained record (timestamps, ordinals, flags). */
const RECORD_BYTES = 64
/** Links released locally whose `link` record may still be in flight. */
const RETIRED_LINK_CAPACITY = 256

export interface ReactNativeRustCoreProviderOptions {
  /** Target mobile platform (selects the adapter identity and capabilities). */
  readonly platform: ReactNativeRustCorePlatform
  /**
   * The native Rust core binding. There is no TypeScript fallback: a
   * missing or foreign binding fails before any radio work.
   */
  readonly binding: ReactNativeRustCoreBinding
  /** Owner label for admitted session leases (host identity). */
  readonly owner: string
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Runtime facts about the host OS (Android API level). */
  readonly runtime: ReactNativeRustCoreRuntimeFacts
  /**
   * The app-declared restoration authority (from `restorationIdentity`), or
   * `null` when the app configured none. Asked at adoption time.
   */
  readonly restorationAuthority?: () => ReactNativeRestorationAuthority | null
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /**
   * The manager's bounded diagnostic trace (`diagnostics.traceMaximumRecords`
   * / `traceMaximumBytes`). Every operation the backend sends to the owner
   * records its dispatch and outcome here; absent, nothing is traced.
   */
  readonly trace?: CoreTraceSink
}

export interface ReactNativeRustCoreBackendProvider extends ReactNativeRestorationBackendProvider {
  create(selection: AdapterSelection<string>): Promise<ReactNativeRustCoreBackend>
}

let nextOwner = 1
/** Legacy numbered backend instances per process from 1 (`corebluetooth-backend.ts` `allocateBackendInstance`). */
let nextBackendInstance = 1

function allocateBackendInstance(): number {
  const ordinal = nextBackendInstance
  nextBackendInstance += 1
  return ordinal
}

function allocateOwnerId(): string {
  const ordinal = nextOwner
  nextOwner += 1
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
 * The React Native provider over the Rust mobile owner. `listAdapters`
 * opens a probe session and disposes it; `create` opens the backend.
 */
export function createReactNativeRustCoreBackendProvider(
  options: ReactNativeRustCoreProviderOptions
): ReactNativeRustCoreBackendProvider {
  if (options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-rust-core.provider.owner')
  }
  const binding = resolveReactNativeRustCoreBinding(options.binding)
  const createOwnerId = options.createOwnerId ?? allocateOwnerId
  const journalHost: { backend: ReactNativeRustCoreBackend | null } = { backend: null }
  const journal = new RustCoreRestorationJournal({
    platform: options.platform,
    authority: options.restorationAuthority ?? (() => null),
    attachment: () => journalHost.backend?.identity.attachment ?? null,
    claimRestoredPeers: (maxPeers: number) => {
      const backend = journalHost.backend
      if (backend === null) {
        return Promise.reject(contractError('lifecycle.destroyed', 'restoration', 'react-native-rust-core.restoration'))
      }
      return backend.claimRestoredPeers(maxPeers)
    }
  })
  const restoration = new ReactNativeRestorationCoordinator(journal)
  return Object.freeze({
    descriptor: Object.freeze({
      providerId: 'unified-ble:react-native-rust-core-provider',
      hostKind: 'native-mobile',
      loadability: 'loadable',
      compatibility: compatibilityFor(options.platform)
    }),
    restoration,
    listAdapters: async () => {
      const backend = await openBackend(options, binding, createOwnerId(), null)
      const adapters = Object.freeze([backend.identity.attachment.adapter])
      const cleanup = await backend.destroy()
      if (cleanup.state !== 'released') {
        throw cleanupError(cleanup, 'react-native-rust-core.provider.list-adapters.cleanup')
      }
      return adapters
    },
    create: async (selection: AdapterSelection<string>) => {
      if (String(selection.selectedAdapterId) !== String(defaultAdapterIdFor(options.platform))) {
        throw contractError('adapter.unavailable', 'adapter', 'react-native-rust-core.provider.select-adapter')
      }
      const backend = await openBackend(options, binding, createOwnerId(), { restoration, journalHost })
      return backend
    }
  })
}

function cleanupError(cleanup: CleanupRecord, operation: string): BackendContractError {
  const first = cleanup.failures[0]?.error
  return first === undefined
    ? contractError('lifecycle.invariant-violation', 'cleanup', operation)
    : new BackendContractError({ ...first, retryability: 'never' })
}

async function openBackend(
  options: ReactNativeRustCoreProviderOptions,
  binding: ReactNativeRustCoreBinding,
  ownerId: string,
  restoration: {
    readonly restoration: ReactNativeRestorationCoordinator
    readonly journalHost: { backend: ReactNativeRustCoreBackend | null }
  } | null
): Promise<ReactNativeRustCoreBackend> {
  if (ownerId.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-rust-core.provider.owner-id')
  }
  const session = await binding.openSession(`${options.owner}/${ownerId}`)
  let backend: ReactNativeRustCoreBackend
  try {
    const state = await session.invoke('adapter.state', {})
    backend = new ReactNativeRustCoreBackend(
      options.platform,
      session,
      options.now,
      options.runtime,
      state,
      options.trace ?? null,
      leaseId => releaseBackgroundThroughModule(binding, `${options.owner}/${ownerId}/background`, leaseId)
    )
  } catch (error) {
    await disposeUnopenedSession(session, error)
    throw error
  }
  try {
    await backend.open()
    if (restoration !== null) {
      backend.activateRestoration(restoration.restoration)
      restoration.journalHost.backend = backend
    }
    return backend
  } catch (error) {
    const cleanup = await backend.destroy()
    if (cleanup.state !== 'released') {
      throw withCleanupDetail(error, cleanup)
    }
    throw error
  }
}

/** Disposes a session no backend owns yet; a failed disposal travels with the error. */
/**
 * A foreground-service lease belongs to the native module, not to the
 * manager that acquired it (87/N8): after that manager is destroyed its
 * handle still releases the lease through a short-lived session of the same
 * module, as the legacy module released it directly.
 */
async function releaseBackgroundThroughModule(
  binding: ReactNativeRustCoreBinding,
  owner: string,
  leaseId: string
): Promise<CleanupRecord> {
  const session = await binding.openSession(owner)
  let record: CleanupRecord
  try {
    record = cleanupRecordFrom(await session.invoke('background.release', { leaseId }))
  } catch (error) {
    await disposeUnopenedSession(session, error)
    throw error
  }
  const disposed = cleanupRecordFrom(await session.invoke('session.dispose', {}))
  if (disposed.state !== 'released') {
    throw cleanupError(disposed, 'react-native-rust-core.background.release.session')
  }
  await session.close()
  return record
}

function securityKey(state: WireSecurityState): string {
  return JSON.stringify([
    state.bond,
    state.encryption,
    state.authentication,
    state.secureConnections,
    state.pairingPossible
  ])
}

async function disposeUnopenedSession(session: ReactNativeRustCoreSession, cause: unknown): Promise<void> {
  try {
    const record = await session.invoke('session.dispose', {})
    if (record.state === 'released') await session.close()
    else throw withCleanupDetail(cause, cleanupRecordFrom(record))
  } catch (error) {
    if (error === cause) throw error
    throw withCleanupDetail(cause, {
      state: 'release-failed',
      failures: [cleanupFailure('session', error, 'react-native-rust-core.provider.open.cleanup')]
    })
  }
}

function withCleanupDetail(error: unknown, cleanup: CleanupRecord): unknown {
  if (!(error instanceof BackendContractError) || cleanup.state === 'released') return error
  return new BackendContractError({
    ...error.normalized,
    platform: {
      domain: 'react-native-rust-core',
      code: 'cleanup-debt',
      safeMessage: 'the session opened for this backend could not be released',
      metadata: Object.freeze({
        original: error.normalized.platform?.safeMessage ?? null,
        failures: Object.freeze(cleanup.failures.map(failure => `${failure.resourceKind}:${failure.error.code}`))
      })
    }
  })
}

// -- shared helpers -----------------------------------------------------------

function unwrap<Value>(result: WireResult<Value>): Value {
  if (!result.ok) throw result.error
  return result.value
}

function normalizedFrom(error: unknown, operation: string): NormalizedBleError {
  if (error instanceof BackendContractError) return error.normalized
  return contractError('platform.failure', 'core', operation, {
    domain: 'react-native-rust-core',
    code: 'unexpected-failure',
    safeMessage: error instanceof Error ? error.message.slice(0, 1024) : String(error).slice(0, 1024),
    metadata: Object.freeze({})
  }).normalized
}

function cleanupFailure(resourceKind: string, error: unknown, operation: string): CleanupFailure {
  return Object.freeze({ resourceKind, error: normalizedFrom(error, operation) })
}

/** The owner's cleanup record as the contract's `CleanupRecord` (codes and details verbatim). */
function cleanupRecordFrom(record: WireCleanupRecord): CleanupRecord {
  if (record.state === 'released') return Object.freeze({ state: 'released', failures: Object.freeze([]) })
  return Object.freeze({
    state: 'release-failed',
    failures: Object.freeze(
      record.failures.map(failure =>
        Object.freeze({
          resourceKind: failure.resourceKind,
          error: contractError(failure.code, failure.domain, failure.operation, remotePlatformDetail(failure))
            .normalized
        })
      )
    )
  })
}

function mergeCleanup(records: readonly CleanupRecord[]): CleanupRecord {
  const failures = records.flatMap(record => record.failures)
  return failures.length === 0
    ? Object.freeze({ state: 'released', failures: Object.freeze([]) })
    : Object.freeze({ state: 'release-failed', failures: Object.freeze(failures) })
}

const RELEASED: CleanupRecord = Object.freeze({ state: 'released', failures: Object.freeze([]) })

function utf8Length(text: string): number {
  let bytes = 0
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index)
    if (code < 0x80) bytes += 1
    else if (code < 0x800) bytes += 2
    else if (code >= 0xd800 && code <= 0xdbff) {
      bytes += 4
      index += 1
    } else bytes += 3
  }
  return bytes
}

function present<Value>(value: Value | null, reason: string): AdvertisementField<Value> {
  if (value === null) {
    return Object.freeze({ state: 'absent', reason, provenance: 'not-provided' })
  }
  return Object.freeze({ state: 'present', value, provenance: 'observed' })
}

const ABSENT_EMPTY_OR_UNREPORTED = 'empty-or-absent-indistinguishable'

/** Core property bits (`ubm_core` GATT_PROP_*): READ 0x01, WRITE 0x02, WRITE_NO_RSP 0x04, NOTIFY 0x08, INDICATE 0x10. */
function characteristicPropertiesFromBits(bits: number): CharacteristicProperties {
  return createGattCharacteristicProperties({
    read: (bits & 0x01) !== 0,
    writeWithResponse: (bits & 0x02) !== 0,
    writeWithoutResponse: (bits & 0x04) !== 0,
    notify: (bits & 0x08) !== 0,
    indicate: (bits & 0x10) !== 0
  })
}

/** Stable, non-reversible reference token for a bonded native id (legacy Android peer directory). */
function stablePeerToken(nativePeerId: string): string {
  let first = 0x811c9dc5
  let second = 0x9e3779b9
  for (let index = 0; index < nativePeerId.length; index += 1) {
    const code = nativePeerId.charCodeAt(index)
    first = Math.imul(first ^ code, 0x01000193)
    second = Math.imul(second ^ (code + index), 0x01000193)
  }
  return `android-bonded-${(first >>> 0).toString(16).padStart(8, '0')}-${(second >>> 0).toString(16).padStart(8, '0')}`
}

function linkKey(nativePeerId: string, connectionGeneration: string): string {
  return `${nativePeerId}\u0000${connectionGeneration}`
}

// -- retained state -------------------------------------------------------------

type CharacteristicSelector = {
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence: number
}

type DescriptorSelector = CharacteristicSelector & {
  readonly descriptorUuid: string
  readonly descriptorOccurrence: number
}

interface ScanConsumer {
  readonly leaseId: LeaseId<string, string>
  readonly options: OwnerScanOptions<string, string>
  readonly filter: ScanFilter
  readonly stream: OwnedCoreBoundedStream<AdvertisementObservation<string>>
  readonly seenPeers: Set<string>
  /** Advertisements the owner dropped at native ingress while this consumer was live (cumulative). */
  ingressDropped: number
}

interface ScanGroup {
  /** The owner's membership id (`s{n}-scan-{k}`), retained until release is confirmed. */
  readonly membership: string
  readonly scanSessionId: ScanSessionId<string, string>
  readonly ownerLeaseId: LeaseId<string, string>
  readonly shareToken: ScanShareToken<string, string> | null
  readonly consumers: Map<string, ScanConsumer>
  state: 'active' | 'stopping' | 'release-failed' | 'released'
  stopping: Promise<CleanupRecord> | null
  deadlineTimer: ReturnType<typeof setTimeout> | null
  removeAbort: (() => void) | null
}

interface ConnectionEntry {
  readonly key: string
  readonly resource: BackendConnection<string, string>
  readonly nativePeerId: string
  /** The core lease name (`connection.connect` `lease`). */
  readonly lease: string
  /** The core-issued connection generation. */
  readonly coreGeneration: string
  linkState: 'connected' | 'lost'
  release: Promise<CleanupRecord> | null
  released: boolean
  readonly databases: Set<string>
}

interface DatabaseEntry {
  readonly key: string
  readonly connectionKey: string
  readonly coreGeneration: string
  readonly path: DatabasePath<string, string, string>
  readonly discovery: WireDiscovery
  valid: boolean
}

interface SubscriptionEntry {
  readonly consumer: string
  readonly subscriptionId: ReturnType<AttachmentBoundIdFactory<string>['subscriptionId']>
  readonly nativePeerId: string
  readonly connectionKey: string
  readonly selector: CharacteristicSelector
  readonly stream: OwnedCoreBoundedStream<NotificationValue>
  /** `ended`: the owner retired the consumer itself (`stream-end`); nothing remains to release. */
  state: 'subscribing' | 'active' | 'ended'
  removal: Promise<OperationTerminalRecord<string, string>> | null
  /** Notifications the owner dropped at native ingress while this consumer was live (cumulative). */
  ingressDropped: number
}

/** A stream that accounts for records its source lost before routing them. */
interface IngressLossAccount {
  readonly stream: Pick<OwnedCoreBoundedStream<unknown>, 'observeSourceOverflow'>
  ingressDropped: number
}

/** Why the owner invalidated a peer's streams, as the next `stream-end` should say. */
type InvalidationReason = Extract<CoreStreamTerminalReason, 'connection-lost' | 'service-changed' | 'source-failed'>

// -- the backend -------------------------------------------------------------------

/**
 * One admitted session projected onto the backend contract. Everything that
 * reaches a radio crosses `session.invoke`; everything the radio reports
 * arrives through `deliver`.
 */
export class ReactNativeRustCoreBackend implements BleCentralBackend<string, NativeBackendIdentity<string>> {
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly peers: PeerDirectoryBackend<string>
  readonly features: FeatureRegistry
  readonly security: RustCoreSecurityBackend | undefined
  /** Session services the Expo layer reaches through the manager (background, companion). */
  readonly hostServices: ReactNativeRustCoreHostServices

  private readonly backendInstanceId: BackendInstanceId<string>
  private attachmentRecord: AttachmentRecord<string>
  private identifiers: AttachmentBoundIdFactory<string>
  private readonly router: RustCoreDrainRouter
  private readonly peerIdsByNativeId = new Map<string, PeerId<string>>()
  private readonly nativeIdsByPeerId = new Map<string, string>()
  private readonly scanGroups = new Map<string, ScanGroup>()
  private readonly connectionsByKey = new Map<string, ConnectionEntry>()
  private readonly connectionsByLink = new Map<string, ConnectionEntry>()
  /**
   * Acquisitions in flight per native peer (finding 194). A caller above the
   * provider can abandon a connect — no abort, no deadline — leaving the
   * owner's `Connecting` claim live; a newer connect for the same peer then
   * supersedes it (cancel plus settle) so arbitration admits the retry
   * instead of refusing `connection.already-owned`. Mirrors the desktop
   * provider's pending-acquisition supersede.
   */
  private readonly pendingAcquisitions = new Map<string, { operationId: string; settled: Promise<void> }>()
  private readonly retiredLinks = new Set<string>()
  private readonly databases = new Map<string, DatabaseEntry>()
  private readonly subscriptions = new Map<string, SubscriptionEntry>()
  private readonly invalidations = new Map<string, InvalidationReason>()
  private readonly eventStreams = new Set<OwnedCoreBoundedStream<BackendEvent<string>>>()
  private readonly adapterWatches = new Set<OwnedCoreBoundedStream<AdapterStateSnapshot<string>>>()
  private readonly backgroundLeases = new Set<string>()
  private lastCounters: WireCounters | null = null
  private restorationActivation: ReactNativeRestorationActivation | null = null
  private restoration: ReactNativeRestorationCoordinator | null = null
  private destroyed = false
  private destroyResult: Promise<CleanupRecord> | null = null
  private sessionDisposed = false
  private nextOrdinal = 1
  // Legacy per-backend resource counters (origin/main corebluetooth-backend.ts:352-357).
  private nextPeer = 1
  private nextScan = 1
  private nextConnection = 1
  private nextDatabase = 1
  private nextSubscription = 1
  private nextIngressOrdinal = 1

  constructor(
    private readonly platform: ReactNativeRustCorePlatform,
    private readonly session: ReactNativeRustCoreSession,
    private readonly now: () => number,
    runtime: ReactNativeRustCoreRuntimeFacts,
    initialState: WireAdapterState,
    private readonly trace: CoreTraceSink | null = null,
    private readonly releaseModuleBackground: ((leaseId: string) => Promise<CleanupRecord>) | null = null
  ) {
    // Legacy React Native attachment names (origin/main
    // corebluetooth-attachment-lifecycle.ts): the instance is this backend's,
    // the generations are the owner's, the attachment joins the three.
    this.backendInstanceId = opaqueId(
      `react-native-${platform}-backend-${allocateBackendInstance()}`,
      'backend-instance',
      SCOPE
    )
    ;[this.attachmentRecord, this.identifiers] = this.attachmentFor(initialState)
    this.features = createReactNativeRustCoreFeatureRegistry(
      platform,
      REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION,
      runtime,
      Object.freeze({ invoke: (input: MaximumWriteLengthFeatureInput) => this.observeMaximumWriteLength(input) })
    )
    this.security =
      platform === 'android'
        ? new RustCoreSecurityBackend({
            now,
            nativePeerId: (peerId, operation) => this.nativeIdForPeerId(peerId, operation),
            budget: (options, operation) => this.budget(options, operation),
            mintOperationId: kind => this.mintOperationId(kind),
            securityState: args => this.invoke('security.state', args),
            pair: args => this.invoke('security.pair', args),
            cancelPairing: async args => {
              await this.invoke('security.cancel-pairing', args)
            },
            watchAbort: (signal, operationId, operation) => this.watchAbort(signal, operationId, operation)
          })
        : undefined
    this.router = new RustCoreDrainRouter(session, {
      deliver: record => this.deliver(record),
      failed: error => this.drainFailed(error),
      noteControlLoss: total => this.noteControlLoss(total)
    })
    const plan = platform === 'android' ? diagnosticReactNativeAndroidScanPlan : diagnosticReactNativeAppleScanPlan
    this.adapter = Object.freeze({
      currentState: () => this.currentAdapterState(),
      watchState: () => this.watchAdapterState()
    })
    this.scanner = Object.freeze({
      plan: (query: NormalizedScanQuery): ScanPlan => plan(query),
      start: (options: OwnerScanOptions<string, string>, clientId: ClientId<string, string>) =>
        this.startScan(options, clientId),
      join: (
        leaseId: LeaseId<string, string>,
        token: ScanShareToken<string, string>,
        clientId: ClientId<string, string>
      ) => this.joinScan(leaseId, token, clientId)
    })
    this.connections = Object.freeze({
      connect: (peerId: PeerId<string>, clientId: ClientId<string, string>, options: ConnectionOptions) =>
        this.connect(peerId, clientId, options),
      ...(platform === 'android'
        ? { peerFromAddress: (descriptor: PeerAddressDescriptor) => this.peerFromAddress(descriptor) }
        : {}),
      readRssi: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: ReadRssiRequest<string, Operation>
      ) => this.readRssi(connection, request),
      requestMtu: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: RequestMtuRequest<string, Operation>
      ) => this.requestMtu(connection, request),
      effectiveMtu: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: EffectiveMtuRequest<string, Operation>
      ) => this.effectiveMtu(connection, request),
      requestPriority: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: RequestPriorityRequest<string, Operation>
      ) => this.requestPriority(connection, request),
      readPhy: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: ReadPhyRequest<string, Operation>
      ) => this.readPhy(connection, request),
      requestPhy: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: RequestPhyRequest<string, Operation>
      ) => this.requestPhy(connection, request),
      maximumWriteLength: <Operation extends string>(
        connection: BackendConnection<string, string>,
        request: ConnectionMaximumWriteLengthRequest<string, Operation>
      ) => this.maximumWriteLength(connection, request)
    })
    this.gatt = Object.freeze({
      discover: (connection: BackendConnection<string, string>, options: PublicOperationOptions) =>
        this.discover(connection, options),
      read: <Operation extends string>(
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: ReadRequest<string, Operation>
      ) => this.read(path, request),
      write: <Operation extends string>(
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: WriteRequest<string, Operation>
      ) => this.write(path, request),
      readDescriptor: <Operation extends string>(
        path: DescriptorPath<string, string, string, string, string, string, 'current'>,
        request: ReadRequest<string, Operation>
      ) => this.readDescriptor(path, request),
      writeDescriptor: <Operation extends string>(
        path: DescriptorPath<string, string, string, string, string, string, 'current'>,
        request: WriteRequest<string, Operation>
      ) => this.writeDescriptor(path, request),
      subscribe: <Operation extends string>(
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: SubscribeRequest<string, Operation>
      ) => this.subscribe(path, request),
      unsubscribe: <Operation extends string>(
        subscription: BackendSubscription<string, string, string, string, string>,
        operation: OperationOptions<string, Operation>
      ) => this.unsubscribe(subscription, operation)
    })
    this.peers = Object.freeze({
      resolve: (reference: PeerReference, options: BackendPeerQuery) => this.resolvePeer(reference, options),
      known: (options: BackendPeerQuery) => this.listPeers('peers.known', 'known', options),
      connected: (options: BackendPeerQuery) => this.listPeers('peers.connected', 'connected', options),
      bonded: (options: BackendPeerQuery) => this.bondedPeers(options),
      authorized: (_options: BackendPeerQuery) =>
        Promise.reject(contractError('capability.unsupported', 'connection', `${SCOPE}.peers.authorized`)),
      restored: (options: BackendPeerQuery) => this.restoredPeers(options)
    })
    this.hostServices = Object.freeze({
      acquireBackground: (request: { readonly kind: 'connected-device'; readonly reason: string }) =>
        this.acquireBackground(request),
      releaseBackground: (leaseId: string) => this.releaseBackground(leaseId),
      updateBackgroundNotification: (request: {
        readonly leaseId: string
        readonly title: string
        readonly body?: string
      }) => this.updateBackgroundNotification(request),
      associateCompanion: (request: { readonly name?: string; readonly serviceUuid?: string }) =>
        this.associateCompanion(request),
      observePresence: (request: { readonly peerId: string }) => this.observePresence(request),
      unobservePresence: (request: { readonly peerId: string }) => this.unobservePresence(request),
      counters: () => this.describeCounters()
    })
  }

  /** Starts delivery (one drain collects anything queued before) and loads the counters. */
  async open(): Promise<void> {
    this.router.start()
    await this.refreshCounters()
  }

  activateRestoration(restoration: ReactNativeRestorationCoordinator): void {
    this.restoration = restoration
    this.restorationActivation = restoration.activate(this.attachmentRecord, this.nativeVersions())
  }

  get identity(): NativeBackendIdentity<string> {
    return Object.freeze({
      registeredBackendId: backendIdFor(this.platform),
      registeredPlatformId: platformIdFor(this.platform),
      attachment: this.attachmentRecord,
      versions: this.nativeVersions(),
      runtime: Object.freeze({
        hostKind: 'native-mobile',
        implementationVersion: REACT_NATIVE_RUST_CORE_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          boundary: 'ubm-mobile-wire/1',
          transport: 'native-core-session',
          sessionId: this.session.sessionId,
          nativeBinding: this.session.buildIdentity.binding,
          nativeTarget: this.session.buildIdentity.target
        })
      })
    })
  }

  /** One attachment per backend, after the caller's core version offer negotiates (legacy rule). */
  async attach(request: BackendAttachmentRequest): Promise<BackendAttachment<string, NativeBackendIdentity<string>>> {
    this.assertOperational(`${SCOPE}.attach`)
    if (this.attached) throw contractError('lifecycle.invalid-state', 'core', `${SCOPE}.attach`)
    negotiateCoreVersions(compatibilityFor(this.platform), request.coreCompatibility)
    this.attached = true
    return Object.freeze({ attachment: this.attachmentRecord, identity: this.identity })
  }

  private attached = false

  events(): BoundedAsyncStream<BackendEvent<string>> {
    this.assertOperational(`${SCOPE}.events`)
    const stream: OwnedCoreBoundedStream<BackendEvent<string>> = new OwnedCoreBoundedStream<BackendEvent<string>>(
      { itemCapacity: capacity(256), byteCapacity: capacity(262144), reservedControlCapacity: capacity(1024) },
      'error',
      () => this.eventStreams.delete(stream)
    )
    this.eventStreams.add(stream)
    return stream
  }

  /**
   * This manager's counters (the resources its session lease holds on the
   * process owner) as of the last settled resource operation (every
   * operation that acquires or releases a resource refreshes them before it
   * resolves). Other managers' resources are not counted here; the process
   * totals are `describeCounters().process`.
   */
  resourceCounters(): ResourceCounters {
    const counters = this.lastCounters
    if (counters === null) {
      throw contractError('lifecycle.invariant-violation', 'core', `${SCOPE}.counters-unavailable`)
    }
    const owned = counters.counters
    return Object.freeze({
      activeScanControllers: resourceCount(owned.activeScanControllers),
      scanConsumers: resourceCount(owned.scanConsumers),
      chooserSessions: resourceCount(owned.chooserSessions),
      connectionLeases: resourceCount(owned.connectionLeases),
      physicalLinks: resourceCount(owned.physicalLinks),
      databaseSnapshots: resourceCount(owned.databaseSnapshots),
      physicalCccdEnablements: resourceCount(owned.physicalCccdEnablements),
      subscriptionConsumers: resourceCount(owned.subscriptionConsumers),
      queuedOperations: resourceCount(owned.queuedOperations),
      dispatchedOperations: resourceCount(owned.dispatchedOperations),
      retainedByteBuffers: resourceCount(owned.retainedByteBuffers + this.retainedJsBytes()),
      restorationRecords: resourceCount(owned.restorationRecords),
      orphanedIpcOwners: resourceCount(owned.orphanedIpcOwners)
    })
  }

  /** The session's full counter record, native half and the explicitly named process totals included. */
  async describeCounters(): Promise<WireCounters> {
    this.assertOperational(`${SCOPE}.counters.describe`)
    await this.refreshCounters()
    const counters = this.lastCounters
    if (counters === null) throw contractError('lifecycle.invariant-violation', 'core', `${SCOPE}.counters-unavailable`)
    return counters
  }

  /**
   * `peers.claim-restored` for the restoration journal: the restored peers
   * this manager adopts. The owner hands each restored peer to one adopter
   * per process (legacy consumed the OS restoration identifiers on the
   * first adoption), so a later manager's claim finds none.
   */
  async claimRestoredPeers(maxPeers: number): Promise<readonly WirePeerRecord[]> {
    this.assertOperational(`${SCOPE}.peers.claim-restored`)
    return (await this.invoke('peers.claim-restored', { maxPeers })).peers
  }

  /** Native id for a public peer id (TCK controller). */
  peerIdForNativeId(nativePeerId: string): string {
    return String(this.peerIdForNative(nativePeerId))
  }

  destroy(): Promise<CleanupRecord> {
    if (this.destroyResult === null) {
      const destruction = this.destroyInternal()
      this.destroyResult = destruction.then(
        cleanup => {
          if (cleanup.state !== 'released') this.destroyResult = null
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

  // -- lifecycle --------------------------------------------------------------------

  private async destroyInternal(): Promise<CleanupRecord> {
    this.destroyed = true
    const records: CleanupRecord[] = []
    if (this.restorationActivation !== null && this.restoration !== null) {
      await this.restoration.deactivate(this.restorationActivation)
      this.restorationActivation = null
    }
    if (!this.sessionDisposed) {
      let disposal: CleanupRecord
      try {
        disposal = cleanupRecordFrom(await this.session.invoke('session.dispose', {}))
      } catch (error) {
        disposal = {
          state: 'release-failed',
          failures: [cleanupFailure('session', error, `${SCOPE}.session.dispose`)]
        }
      }
      if (disposal.state !== 'released') {
        // The lease stays open so a retried destroy can dispose it again.
        return disposal
      }
      this.sessionDisposed = true
      await this.refreshCounters()
    }
    await this.router.stop()
    this.retireLocalState('owner-released')
    try {
      await this.session.close()
    } catch (error) {
      records.push({ state: 'release-failed', failures: [cleanupFailure('session', error, `${SCOPE}.session.close`)] })
    }
    for (const stream of [...this.eventStreams]) stream.closeWithReason('owner-released')
    this.eventStreams.clear()
    return mergeCleanup(records)
  }

  /** Ends every JS-side stream and forgets the handles (the owner released their resources). */
  private retireLocalState(reason: CoreStreamTerminalReason, error: NormalizedBleError | null = null): void {
    for (const group of this.scanGroups.values()) this.endScanGroup(group, reason, error)
    this.scanGroups.clear()
    for (const entry of this.subscriptions.values()) {
      entry.state = 'ended'
      entry.stream.closeWithReason(reason, error)
    }
    this.subscriptions.clear()
    for (const watch of [...this.adapterWatches]) watch.closeWithReason(reason, error)
    this.adapterWatches.clear()
    this.security?.close()
    for (const entry of this.connectionsByKey.values()) entry.released = true
    this.connectionsByKey.clear()
    this.connectionsByLink.clear()
    this.databases.clear()
    this.backgroundLeases.clear()
  }

  private drainFailed(error: unknown): void {
    // The session can no longer report platform facts: every stream ends
    // with the owner's failure rather than waiting forever.
    const normalized = normalizedFrom(error, `${SCOPE}.drain`)
    this.trace?.record({
      timestamp: this.now(),
      resource: 'manager',
      transition: 'source-failed',
      operation: null,
      cause: normalized.code,
      queuedOperations: 0,
      dispatchedOperations: this.tracedInFlight,
      quarantinedOperations: 0
    })
    for (const group of this.scanGroups.values()) this.endScanGroup(group, 'source-failed', normalized)
    for (const entry of this.subscriptions.values()) entry.stream.closeWithReason('source-failed', normalized)
    for (const watch of [...this.adapterWatches]) watch.closeWithReason('source-failed', normalized)
    for (const stream of [...this.eventStreams]) stream.closeWithReason('source-failed', normalized)
  }

  private assertOperational(operation: string): void {
    if (this.destroyed) throw contractError('lifecycle.destroyed', 'core', operation)
  }

  private nativeVersions(): NativeVersionAxes {
    const compatibility = compatibilityFor(this.platform)
    return Object.freeze({
      ...negotiateCoreVersions(compatibility, compatibility),
      nativeProtocol: negotiateVersion(compatibility.nativeProtocol, compatibility.nativeProtocol)
    })
  }

  private invoke<Op extends WireOp>(op: Op, rawArgs: WireJsonObject): Promise<WireOpResults[Op]> {
    let args: WireJsonObject
    try {
      args = this.admit(op, rawArgs)
    } catch (error) {
      return Promise.reject(error)
    }
    const trace = this.trace
    if (trace === null) return this.session.invoke(op, args)
    // Payload-free: the label is a per-capture ordinal, never the op's peer,
    // path or bytes (trace-format.ts).
    const label = `operation-${this.nextTraceLabel}`
    this.nextTraceLabel += 1
    this.tracedInFlight += 1
    this.recordOperation(trace, label, 'dispatched', null)
    return this.session.invoke(op, args).then(
      value => {
        this.tracedInFlight -= 1
        this.recordOperation(trace, label, 'succeeded', null)
        return value
      },
      (error: unknown) => {
        this.tracedInFlight -= 1
        const code = normalizedFrom(error, `${SCOPE}.${op}`).code
        this.recordOperation(trace, label, traceOutcomeFor(code), code)
        throw error
      }
    )
  }

  /**
   * Every operation the owner can cancel, from its abort watch until it
   * settles: the admission it was sent with (`null` until sent) and whether
   * it was cancelled before it was sent (finding 109).
   */
  private readonly pendingOperations = new Map<string, { admission: number | null; cancelledBeforeSend: boolean }>()
  private nextAdmission = 0

  /**
   * Stamp the wire `admission` (finding 109): every invoke naming an
   * operation carries the session's next admission, assigned here in send
   * order (the native invoke is issued synchronously after this). An
   * operation cancelled before it was sent is refused here, with no effect.
   * `op.cancel` names its target's admission; `scan.stop`'s `operationId`
   * names a scan membership.
   */
  private admit(op: WireOp, args: WireJsonObject): WireJsonObject {
    const operationId = args.operationId
    if (op === 'op.cancel' || op === 'scan.stop' || typeof operationId !== 'string') return args
    const pending = this.pendingOperations.get(operationId)
    if (pending?.cancelledBeforeSend === true) {
      throw contractError('operation.aborted', 'core', `${SCOPE}.${op}`)
    }
    this.nextAdmission += 1
    if (pending !== undefined) pending.admission = this.nextAdmission
    return { ...args, admission: this.nextAdmission }
  }

  private nextTraceLabel = 1
  private tracedInFlight = 0

  private recordOperation(
    trace: CoreTraceSink,
    label: string,
    transition: string,
    cause: NormalizedBleError['code'] | null
  ): void {
    trace.record({
      timestamp: this.now(),
      resource: 'operation',
      transition,
      operation: label,
      cause,
      queuedOperations: 0,
      dispatchedOperations: this.tracedInFlight,
      quarantinedOperations: 0
    })
  }

  private async refreshCounters(): Promise<void> {
    try {
      this.lastCounters = await this.session.invoke('counters.describe', {})
    } catch (error) {
      if (this.lastCounters === null) throw error
      // The operation that asked for the refresh already settled; the stale
      // snapshot stays, and the failure is reported, not dropped.
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'counters-refresh-failed',
        message: 'The Rust owner did not answer counters.describe after a resource operation',
        detail: Object.freeze({ code: normalizedFrom(error, `${SCOPE}.counters`).code })
      })
    }
  }

  private retainedJsBytes(): number {
    let bytes = 0
    for (const group of this.scanGroups.values()) {
      for (const consumer of group.consumers.values()) bytes += consumer.stream.retainedPayloadBytes()
    }
    for (const entry of this.subscriptions.values()) bytes += entry.stream.retainedPayloadBytes()
    return bytes
  }

  // -- operation plumbing ------------------------------------------------------------

  /**
   * The owner's operation id for one wire invoke. Internal to the wire:
   * public results carry the caller's correlation (`publicCorrelation`).
   */
  private mintOperationId(kind: string): string {
    const ordinal = this.nextOrdinal
    this.nextOrdinal += 1
    return `${kind}-${ordinal}`
  }

  private nextPublicOperation = 1

  /**
   * The correlation of one public operation, as the legacy core minted it
   * (origin/main `src/core/unified-ble-core.ts:179`, `operation-{n}` from one
   * per-manager counter): reads, writes, descriptor reads and writes,
   * subscribes, connection controls and one per long write.
   */
  publicCorrelation(): OperationCorrelation<string, string> {
    const ordinal = this.nextPublicOperation
    this.nextPublicOperation += 1
    return this.identifiers.operationCorrelation(`operation-${ordinal}`)
  }

  /**
   * The caller's deadline as the owner's relative `budgetMs`. An expired
   * deadline never reaches the owner: the operation times out here, before
   * any effect.
   */
  private budget(options: PublicOperationOptions, operation: string): { readonly budgetMs?: number } {
    if (options.signal?.aborted === true) throw contractError('operation.aborted', 'core', operation)
    if (options.deadline === null || options.deadline === undefined) return {}
    const remaining = Math.floor(Number(options.deadline) - this.now())
    if (remaining <= 0) throw contractError('operation.timed-out', 'core', operation)
    return { budgetMs: Math.min(remaining, MAX_BUDGET_MS) }
  }

  /**
   * Exact cancellation (finding 109): a settled operation is already
   * terminal; one not sent yet is refused when it would be sent; a sent one
   * is cancelled by the owner, which classifies it by its admission.
   */
  private async cancel(operationId: string): Promise<CancellationAcknowledgement<string>> {
    const handle = this.identifiers.backendOperationHandle(operationId)
    const pending = this.pendingOperations.get(operationId)
    if (pending === undefined) return Object.freeze({ handle, state: 'already-terminal' })
    if (pending.admission === null) {
      pending.cancelledBeforeSend = true
      return Object.freeze({ handle, state: 'cancellation-requested' })
    }
    const answer = await this.invoke('op.cancel', { operationId, admission: pending.admission })
    return Object.freeze({ handle, state: answer.state })
  }

  /** An abort-driven cancel: the operation settles on its own; a refused cancel is reported. */
  private cancelDetached(operationId: string, operation: string): void {
    this.cancel(operationId).catch((error: unknown) => {
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'cancel-failed',
        message: `op.cancel for ${operation} was not accepted`,
        // The owner's operation id is wire-internal; `operation` names it.
        detail: Object.freeze({ code: normalizedFrom(error, `${SCOPE}.op.cancel`).code })
      })
    })
  }

  /** Tracks `operationId` until it settles and cancels it when `signal` aborts. */
  private watchAbort(signal: AbortSignal | null, operationId: string, operation: string): () => void {
    this.pendingOperations.set(operationId, { admission: null, cancelledBeforeSend: false })
    const onAbort = (): void => this.cancelDetached(operationId, operation)
    signal?.addEventListener('abort', onAbort, { once: true })
    return () => {
      this.pendingOperations.delete(operationId)
      signal?.removeEventListener('abort', onAbort)
    }
  }

  /**
   * Runs one owner operation linked to `operationId`: the abort listener is
   * attached before dispatch, removed when it settles; the dispatch's cancel
   * sends `op.cancel` for exactly this operation.
   */
  private dispatch<Result>(
    operationId: string,
    signal: AbortSignal | null,
    operation: string,
    run: () => Promise<Result>
  ): BackendOperationDispatch<string, Result> {
    const removeAbort = this.watchAbort(signal, operationId, operation)
    const completion = (async () => {
      try {
        return await run()
      } finally {
        removeAbort()
      }
    })()
    return createBackendOperationDispatch(this.identifiers.backendOperationHandle(operationId), completion, () =>
      this.cancel(operationId)
    )
  }

  private terminal(
    correlation: OperationOptions<string, string>['correlation']
  ): OperationTerminalRecord<string, string> {
    return Object.freeze({ correlation, outcome: 'succeeded', cause: null })
  }

  private emitEvent(
    event: DistributiveOmit<BackendEvent<string>, 'attachment' | 'attachmentId' | 'ingressOrdinal'>
  ): void {
    const ingressOrdinal = this.nextIngressOrdinal
    this.nextIngressOrdinal += 1
    const full = Object.freeze({
      ...event,
      attachment: this.attachmentRecord,
      attachmentId: this.attachmentRecord.attachmentId,
      ingressOrdinal
    }) as BackendEvent<string>
    const bytes = RECORD_BYTES + utf8Length(JSON.stringify(event, jsonSafe))
    for (const stream of [...this.eventStreams]) stream.emit(full, bytes)
  }

  // -- adapter ---------------------------------------------------------------------------

  private snapshotFrom(
    state: WireAdapterState,
    backendGeneration: AttachmentRecord<string>['backendGeneration']
  ): AdapterStateSnapshot<string> {
    return Object.freeze({
      availability: state.availability,
      authorization: state.authorization,
      power: state.power,
      backendGeneration,
      updatedAt: monotonicTimestamp(state.updatedAt),
      safeReason: state.safeReason
    })
  }

  /**
   * The attachment under the owner's generations in `state`, with the
   * adapter's latest state (legacy `buildAttachment`, rebuilt on every
   * adapter state and generation advance).
   */
  private attachmentFor(state: WireAdapterState): [AttachmentRecord<string>, AttachmentBoundIdFactory<string>] {
    const backendInstanceId = this.backendInstanceId
    const attachmentId = opaqueId(
      `${String(backendInstanceId)}:${state.backendGeneration}:${state.adapterGeneration}`,
      'attachment',
      SCOPE
    )
    const backendGeneration = opaqueId(state.backendGeneration, 'backend-generation', SCOPE)
    const adapterId = opaqueId(adapterNativeIdFor(this.platform), 'adapter', SCOPE)
    const adapterGeneration = opaqueId(state.adapterGeneration, 'adapter-generation', SCOPE)
    const record: AttachmentRecord<string> = Object.freeze({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapter: Object.freeze({
        adapterId,
        displayName:
          this.platform === 'android' ? 'Android default BLE adapter' : 'Apple CoreBluetooth central adapter',
        state: this.snapshotFrom(state, backendGeneration),
        adapterGeneration,
        limitations: Object.freeze([
          'The process-owned Rust mobile owner schedules every radio operation; this backend holds no TypeScript radio policy'
        ])
      })
    })
    const identifiers = createAttachmentBoundIdFactory<string>({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapterId,
      adapterGeneration
    })
    return [record, identifiers]
  }

  private sameGenerations(state: WireAdapterState): boolean {
    return (
      state.backendGeneration === String(this.attachmentRecord.backendGeneration) &&
      state.adapterGeneration === String(this.attachmentRecord.adapter.adapterGeneration)
    )
  }

  private async currentAdapterState(): Promise<AdapterStateSnapshot<string>> {
    this.assertOperational(`${SCOPE}.adapter.state`)
    const state = await this.invoke('adapter.state', {})
    this.observeGenerations(state)
    return this.snapshotFrom(state, opaqueId(state.backendGeneration, 'backend-generation', SCOPE))
  }

  private async watchAdapterState(): Promise<AdapterStateWatch<string>> {
    const initial = await this.currentAdapterState()
    const transitions: OwnedCoreBoundedStream<AdapterStateSnapshot<string>> = new OwnedCoreBoundedStream<
      AdapterStateSnapshot<string>
    >(
      { itemCapacity: capacity(16), byteCapacity: capacity(4096), reservedControlCapacity: capacity(512) },
      'drop-oldest',
      () => this.adapterWatches.delete(transitions)
    )
    this.adapterWatches.add(transitions)
    return Object.freeze({ initial, transitions })
  }

  /**
   * Adopts the owner's state. A generation the owner retired invalidates
   * the links issued under it, as legacy `advanceGeneration` did: the
   * attachment is rebuilt under the new generations and the backend reports
   * `backend-restarted`. Answers whether it did.
   */
  private observeGenerations(state: WireAdapterState): boolean {
    const restarted = !this.sameGenerations(state)
    ;[this.attachmentRecord, this.identifiers] = this.attachmentFor(state)
    if (!restarted) return false
    // 5.0 keeps peer handles across the advance (legacy cleared them with
    // the generation and destroyed the manager): the peer is the same device,
    // so a supervisor reconnects with the handle it holds.
    for (const entry of this.connectionsByKey.values()) this.markLost(entry)
    return true
  }

  /**
   * Legacy `handleAdapterState` / `advanceGeneration`: watchers see every
   * state; a state change is an `adapter-state` event, a generation advance
   * a `backend-restarted` one.
   */
  private onAdapterRecord(state: WireAdapterState): void {
    const restarted = this.observeGenerations(state)
    const snapshot = this.snapshotFrom(state, opaqueId(state.backendGeneration, 'backend-generation', SCOPE))
    for (const watch of [...this.adapterWatches])
      watch.emit(snapshot, RECORD_BYTES + utf8Length(state.safeReason ?? ''))
    this.emitEvent({ kind: restarted ? 'backend-restarted' : 'adapter-state' })
  }

  // -- peers ---------------------------------------------------------------------------------

  private peerIdForNative(nativePeerId: string): PeerId<string> {
    const existing = this.peerIdsByNativeId.get(nativePeerId)
    if (existing !== undefined) return existing
    const peerId = opaqueId(
      `corebluetooth-peer-${String(this.attachmentRecord.backendGeneration)}-${this.nextPeer}`,
      'peer',
      SCOPE
    )
    this.nextPeer += 1
    this.peerIdsByNativeId.set(nativePeerId, peerId)
    this.nativeIdsByPeerId.set(String(peerId), nativePeerId)
    return peerId
  }

  private nativeIdForPeerId(peerId: string, operation: string): string {
    const native = this.nativeIdsByPeerId.get(peerId)
    if (native === undefined) throw contractError('peer.not-found', 'connection', operation)
    return native
  }

  /** Android address targeting: the canonical address is the native id the radio connects to. */
  private peerFromAddress(descriptor: PeerAddressDescriptor): PeerId<string> {
    if (descriptor.addressType !== 'public' && descriptor.addressType !== 'random') {
      throw contractError('argument.invalid', 'connection', `${SCOPE}.peer-from-address`)
    }
    let address: string
    try {
      address = canonicalBleAddress(descriptor.address)
    } catch {
      throw contractError('argument.invalid', 'connection', `${SCOPE}.peer-from-address`)
    }
    return this.peerIdForNative(address)
  }

  private peerRecord(record: WirePeerRecord, reference: PeerReference): BackendPeerRecord<string> {
    return Object.freeze({
      reference,
      peerId: this.peerIdForNative(record.peerId),
      name: record.name,
      rssi: record.rssi,
      source: record.source,
      state: Object.freeze({
        reachability: record.reachability,
        connection: record.connection,
        bond: record.bond,
        lastSeenAtMonotonicMs: record.lastSeenAtMonotonicMs
      })
    })
  }

  private originReference(nativePeerId: string): PeerReference {
    return Object.freeze({
      version: 1,
      backendId: backendIdFor(this.platform),
      scope: 'origin',
      opaqueId: nativePeerId
    })
  }

  private systemReference(nativePeerId: string): PeerReference {
    return Object.freeze({
      version: 1,
      backendId: backendIdFor(this.platform),
      scope: 'system',
      opaqueId: stablePeerToken(nativePeerId)
    })
  }

  private assertPeerQuery(options: BackendPeerQuery, operation: string): void {
    this.assertOperational(operation)
    if (options.services !== undefined && options.services.length > 0) {
      throw contractError('capability.unsupported', 'connection', `${operation}.services`)
    }
  }

  private filterReferences(
    records: readonly BackendPeerRecord<string>[],
    options: BackendPeerQuery,
    operation: string
  ): readonly BackendPeerRecord<string>[] {
    if (options.references === undefined) return Object.freeze([...records])
    const wanted = new Set(
      options.references.map(reference => {
        assertPeerReference(reference, operation)
        return encodePeerReference(reference)
      })
    )
    return Object.freeze(records.filter(record => wanted.has(encodePeerReference(record.reference))))
  }

  private async resolvePeer(
    reference: PeerReference,
    options: BackendPeerQuery
  ): Promise<BackendPeerRecord<string> | null> {
    const operation = `${SCOPE}.peers.resolve`
    assertPeerReference(reference, operation)
    this.assertPeerQuery(options, operation)
    if (reference.backendId !== backendIdFor(this.platform)) {
      throw contractError('peer.scope-mismatch', 'connection', operation)
    }
    if (reference.scope === 'system') {
      const bonded = await this.bondedPeers({ ...options, references: [reference] })
      return bonded[0] ?? null
    }
    if (reference.scope !== 'origin') throw contractError('peer.scope-mismatch', 'connection', operation)
    const record = await this.invoke('peers.resolve', {
      reference: {
        opaqueId: reference.opaqueId,
        version: reference.version,
        backendId: reference.backendId,
        scope: reference.scope
      }
    })
    if (record === null) return null
    if (options.sources !== undefined && !options.sources.includes(record.source)) return null
    return this.peerRecord(record, this.originReference(record.peerId))
  }

  private async listPeers(
    op: 'peers.known' | 'peers.connected',
    name: string,
    options: BackendPeerQuery
  ): Promise<readonly BackendPeerRecord<string>[]> {
    const operation = `${SCOPE}.peers.${name}`
    this.assertPeerQuery(options, operation)
    const records = (await this.invoke(op, {}))
      .filter(record => options.sources === undefined || options.sources.includes(record.source))
      .map(record => this.peerRecord(record, this.originReference(record.peerId)))
    return this.filterReferences(records, options, operation)
  }

  private async bondedPeers(options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> {
    const operation = `${SCOPE}.peers.bonded`
    this.assertPeerQuery(options, operation)
    if (options.sources !== undefined && !options.sources.includes('system-bonded')) return Object.freeze([])
    const operationId = this.mintOperationId('peers-bonded')
    const removeAbort = this.watchAbort(options.signal ?? null, operationId, operation)
    let records: readonly WirePeerRecord[]
    try {
      records = await this.invoke('peers.bonded', { operationId, ...this.budget(options, operation) })
    } finally {
      removeAbort()
    }
    return this.filterReferences(
      records.map(record => this.peerRecord(record, this.systemReference(record.peerId))),
      options,
      operation
    )
  }

  private async restoredPeers(options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> {
    const operation = `${SCOPE}.peers.restored`
    this.assertPeerQuery(options, operation)
    // Issue #212: Android lists the peers a Companion Device Manager presence
    // wake restored through the same records as iOS state restoration.
    const records = (await this.invoke('peers.restored', {})).map(record =>
      this.peerRecord(record, this.originReference(record.peerId))
    )
    return this.filterReferences(records, options, operation)
  }

  // -- scan ------------------------------------------------------------------------------------

  private nativeScanFilter(options: OwnerScanOptions<string, string>, operation: string): ScanFilter {
    assertScanFilter(options.filter, operation)
    const planScan = this.platform === 'android' ? planReactNativeAndroidScan : planReactNativeAppleScan
    return trustedServiceUuidFilter(options, planScan, operation)
  }

  private scanPlatformArgs(options: OwnerScanOptions<string, string>, operation: string): WireJsonObject {
    const platform = options.platform
    if (platform === undefined) return {}
    if (platform.kind !== 'android' || this.platform !== 'android') {
      throw contractError('capability.unsupported', 'scan', `${operation}.platform-options`)
    }
    if (platform.reportDelayMs !== undefined || platform.phy !== undefined) {
      throw contractError('capability.unsupported', 'scan', `${operation}.platform-options`)
    }
    return {
      platform: {
        ...(platform.mode === undefined ? {} : { mode: platform.mode }),
        ...(platform.callbackType === undefined ? {} : { callbackType: platform.callbackType }),
        ...(platform.legacy === undefined ? {} : { legacy: platform.legacy })
      }
    }
  }

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    const operation = `${SCOPE}.scan.start`
    this.assertOperational(operation)
    // Finding 185: a stop that failed keeps its membership for retry
    // (PR210-09), but nothing ever retried it — every later start then
    // failed scan.already-active until the process died. A start heals
    // retained memberships first by retrying their release under the same
    // identity.
    for (const group of [...this.scanGroups.values()]) {
      if (group.state !== 'release-failed') continue
      const healed = await this.stopScanGroup(group)
      if (healed.state !== 'released') {
        throw contractError('scan.already-active', 'scan', operation, {
          domain: 'react-native-rust-core',
          code: 'scan-release-debt',
          safeMessage: 'a previous scan could not be released; its membership is still active',
          metadata: {
            failures: healed.failures.map(failure => `${failure.resourceKind}:${failure.error.code}`)
          }
        })
      }
    }
    const nativeFilter = this.nativeScanFilter(options, operation)
    const deviceAddresses = (nativeFilter.deviceAddresses ?? []).map(address => canonicalBleAddress(address))
    if (deviceAddresses.length > 0 && this.platform !== 'android') {
      throw contractError('capability.unsupported', 'scan', `${operation}.device-addresses`)
    }
    const operationId = this.mintOperationId('scan')
    const args: WireJsonObject = {
      serviceUuids: nativeFilter.serviceUuids.map(uuid => String(uuid)),
      duplicatePolicy: 'all',
      operationId,
      ...(deviceAddresses.length === 0 ? {} : { deviceAddresses }),
      ...this.scanPlatformArgs(options, operation),
      ...this.budget(options, operation)
    }
    const removeAbort = this.watchAbort(options.signal, operationId, operation)
    let membership: string
    try {
      membership = (await this.invoke('scan.start', args)).operationId
    } catch (error) {
      throw this.describeScanRefusal(error, operation)
    } finally {
      removeAbort()
    }
    const ordinal = this.nextScan
    this.nextScan += 1
    const group: ScanGroup = {
      membership,
      scanSessionId: this.identifiers.scanSessionId(`corebluetooth-scan-session-${ordinal}`),
      ownerLeaseId: this.identifiers.leaseId(`corebluetooth-scan-lease-${ordinal}`),
      shareToken: options.sharing.allowSharing
        ? this.identifiers.scanShareToken(`corebluetooth-scan-share-${ordinal}`)
        : null,
      consumers: new Map(),
      state: 'active',
      stopping: null,
      deadlineTimer: null,
      removeAbort: null
    }
    const owner = this.addScanConsumer(group, group.ownerLeaseId, options)
    this.scanGroups.set(membership, group)
    // The caller's scan duration ends the scan like its abort (legacy rule);
    // `budgetMs` above bounded only the start.
    const end = (): void => {
      this.stopScanGroup(group).then(
        cleanup => {
          if (cleanup.state !== 'released') this.reportScanCleanup(group, cleanup)
        },
        (error: unknown) =>
          this.reportScanCleanup(group, {
            state: 'release-failed',
            failures: [cleanupFailure('scan', error, `${SCOPE}.scan.stop`)]
          })
      )
    }
    if (options.signal !== null) {
      if (options.signal.aborted) end()
      else {
        const signal = options.signal
        signal.addEventListener('abort', end, { once: true })
        group.removeAbort = () => signal.removeEventListener('abort', end)
      }
    }
    if (options.deadline !== null && group.state === 'active') {
      group.deadlineTimer = setTimeout(end, Math.max(0, Number(options.deadline) - this.now()))
    }
    await this.refreshCounters()
    return this.scanLease(group, owner)
  }

  /**
   * Finding 209: the owner refuses a scan while one is active, and its
   * refusal carries no identity. When this provider holds the conflicting
   * scan(s) the error names them (session, owner lease, state) so a scan
   * left open is diagnosable; otherwise the owner's answer stands untouched.
   */
  private describeScanRefusal(error: unknown, operation: string): unknown {
    if (!(error instanceof BackendContractError)) return error
    if (error.normalized.code !== 'scan.already-active') return error
    const active = [...this.scanGroups.values()].filter(group => group.state !== 'released')
    if (active.length === 0) return error
    const sessions = active.map(group => String(group.scanSessionId))
    return contractError('scan.already-active', 'scan', operation, {
      domain: 'react-native-rust-core',
      code: 'scan-arbitration',
      safeMessage:
        active.length === 1
          ? `scan ${sessions.join(', ')} is still active; stop it before starting a new scan`
          : `${active.length.toString()} scans are still active (${sessions.join(', ')}); stop them before starting a new scan`,
      metadata: Object.freeze({
        activeScanSessionIds: Object.freeze([...sessions]),
        ownerLeaseIds: Object.freeze(active.map(group => String(group.ownerLeaseId))),
        states: Object.freeze(active.map(group => group.state))
      })
    })
  }

  private reportScanCleanup(group: ScanGroup, cleanup: CleanupRecord): void {
    this.emitEvent({
      kind: 'diagnostic-warning',
      code: 'scan-cleanup-requires-retry',
      message: 'The scan ended by its signal or deadline could not be released; stop() retries it',
      detail: Object.freeze({
        scanSessionId: String(group.scanSessionId),
        failures: Object.freeze(cleanup.failures.map(failure => failure.error.code))
      })
    })
  }

  private addScanConsumer(
    group: ScanGroup,
    leaseId: LeaseId<string, string>,
    options: OwnerScanOptions<string, string>
  ): ScanConsumer {
    const stream: OwnedCoreBoundedStream<AdvertisementObservation<string>> = new OwnedCoreBoundedStream<
      AdvertisementObservation<string>
    >(options.delivery, options.delivery.overflowPolicy, () => undefined)
    const consumer: ScanConsumer = {
      leaseId,
      options,
      filter: options.filter,
      stream,
      seenPeers: new Set(),
      ingressDropped: 0
    }
    group.consumers.set(String(leaseId), consumer)
    return consumer
  }

  private scanLease(group: ScanGroup, consumer: ScanConsumer): ScanLease<string, string> {
    return Object.freeze({
      scanSessionId: group.scanSessionId,
      leaseId: consumer.leaseId,
      shareToken: consumer.leaseId === group.ownerLeaseId ? group.shareToken : null,
      observations: consumer.stream,
      stop: () => this.stopScanConsumer(group, consumer)
    })
  }

  private async joinScan(
    leaseId: LeaseId<string, string>,
    token: ScanShareToken<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    this.assertOperational(`${SCOPE}.scan.join`)
    const group = [...this.scanGroups.values()].find(
      candidate => candidate.ownerLeaseId === leaseId && candidate.shareToken === token && candidate.state === 'active'
    )
    const owner = group?.consumers.get(String(leaseId))
    if (group === undefined || owner === undefined) {
      throw contractError('ownership.denied', 'scan', `${SCOPE}.scan.join`)
    }
    const ordinal = this.nextScan
    this.nextScan += 1
    const joined = this.addScanConsumer(
      group,
      this.identifiers.leaseId(`corebluetooth-scan-lease-${ordinal}`),
      owner.options
    )
    return this.scanLease(group, joined)
  }

  private async stopScanConsumer(group: ScanGroup, consumer: ScanConsumer): Promise<CleanupRecord> {
    if (consumer.leaseId !== group.ownerLeaseId) {
      group.consumers.delete(String(consumer.leaseId))
      consumer.stream.closeWithReason('owner-released')
      return RELEASED
    }
    return this.stopScanGroup(group)
  }

  /**
   * Stops the membership. Delivery ends at once; the membership id is kept
   * until the owner confirms release, so a failed stop can be retried with the
   * same identity (PR210-09). Concurrent stops share one attempt.
   */
  private stopScanGroup(group: ScanGroup): Promise<CleanupRecord> {
    if (group.state === 'released') return Promise.resolve(RELEASED)
    if (group.stopping !== null) return group.stopping
    this.suspendScanGroup(group)
    group.state = 'stopping'
    const stopping = (async (): Promise<CleanupRecord> => {
      let record: CleanupRecord
      try {
        record = cleanupRecordFrom(await this.invoke('scan.stop', { operationId: group.membership }))
      } catch (error) {
        record = { state: 'release-failed', failures: [cleanupFailure('scan', error, `${SCOPE}.scan.stop`)] }
      }
      if (record.state === 'released') {
        group.state = 'released'
        this.scanGroups.delete(group.membership)
        await this.refreshCounters()
      } else {
        group.state = 'release-failed'
      }
      return record
    })()
    group.stopping = stopping
    stopping.then(
      () => {
        if (group.stopping === stopping) group.stopping = null
      },
      () => {
        if (group.stopping === stopping) group.stopping = null
      }
    )
    return stopping
  }

  private suspendScanGroup(group: ScanGroup): void {
    if (group.deadlineTimer !== null) {
      clearTimeout(group.deadlineTimer)
      group.deadlineTimer = null
    }
    group.removeAbort?.()
    group.removeAbort = null
    for (const consumer of group.consumers.values()) consumer.stream.closeWithReason('owner-released')
  }

  private endScanGroup(group: ScanGroup, reason: CoreStreamTerminalReason, error: NormalizedBleError | null): void {
    if (group.deadlineTimer !== null) {
      clearTimeout(group.deadlineTimer)
      group.deadlineTimer = null
    }
    group.removeAbort?.()
    group.removeAbort = null
    for (const consumer of group.consumers.values()) consumer.stream.closeWithReason(reason, error)
    group.state = 'released'
  }

  private observation(
    record: Extract<WireDrainRecord, { t: 'adv' }>,
    scanSessionId: ScanSessionId<string, string>,
    receivedAt: number,
    ingressOrdinal: number
  ): AdvertisementObservation<string> {
    const peerId = this.peerIdForNative(record.peerId)
    let address: { readonly value: string; readonly type: 'opaque' } | null = null
    try {
      address = Object.freeze({ value: canonicalBleAddress(record.peerId), type: 'opaque' as const })
    } catch {
      address = null
    }
    const uuids = (values: readonly string[] | null): readonly Uuid[] | null =>
      values === null ? null : Object.freeze(values.map(value => canonicalUuid(value)))
    return Object.freeze({
      device: Object.freeze({
        id: peerId,
        backendInstanceId: this.attachmentRecord.backendInstanceId,
        scope: 'session' as const,
        stableAcrossRestarts: false,
        address
      }),
      provenance: 'platform-derived' as const,
      sourceTimestamp: present<SourceTimestamp>(null, 'the owner clock is not the host monotonic clock'),
      receivedAtMonotonicMs: monotonicTimestamp(receivedAt),
      ingressOrdinal,
      scanSessionId,
      localName: present(record.localName, 'not reported by the platform'),
      rssi: present(record.rssi, 'not reported by the platform'),
      txPower: present(record.txPower, 'not reported by the platform'),
      connectable: present(record.connectable, 'not reported by the platform'),
      appearance: present(record.appearance, 'not reported by the platform'),
      serviceUuids: present(uuids(record.serviceUuids), ABSENT_EMPTY_OR_UNREPORTED),
      solicitedServiceUuids: present(uuids(record.solicitedServiceUuids), ABSENT_EMPTY_OR_UNREPORTED),
      overflowServiceUuids: present(uuids(record.overflowServiceUuids), ABSENT_EMPTY_OR_UNREPORTED),
      serviceData: present(
        record.serviceData === null
          ? null
          : Object.freeze(
              record.serviceData.map(entry =>
                Object.freeze({ serviceUuid: canonicalUuid(entry.uuid), value: ownedCopy(entry.payload) })
              )
            ),
        ABSENT_EMPTY_OR_UNREPORTED
      ),
      manufacturerData: present(
        record.manufacturerData === null
          ? null
          : Object.freeze(
              record.manufacturerData.map(entry =>
                Object.freeze({ companyIdentifier: entry.companyId, value: ownedCopy(entry.payload) })
              )
            ),
        ABSENT_EMPTY_OR_UNREPORTED
      ),
      rawRecord: present(
        record.rawRecord === null ? null : ownedCopy(record.rawRecord),
        'not reported by the platform'
      ),
      scanResponseRecord: present<OwnedBytes>(null, 'scan response records are not reported by the mobile owner')
    })
  }

  private onAdvertisement(record: Extract<WireDrainRecord, { t: 'adv' }>): void {
    const bytes = advertisementBytes(record)
    const receivedAt = this.now()
    for (const group of this.scanGroups.values()) {
      if (group.state !== 'active') continue
      const ingressOrdinal = this.nextIngressOrdinal
      this.nextIngressOrdinal += 1
      const observation = this.observation(record, group.scanSessionId, receivedAt, ingressOrdinal)
      for (const consumer of group.consumers.values()) {
        if (consumer.stream.isTerminal()) continue
        if (!advertisementMatchesFilter(consumer.filter, observation)) continue
        if (consumer.options.duplicatePolicy === 'first') {
          if (consumer.seenPeers.has(record.peerId)) continue
          consumer.seenPeers.add(record.peerId)
        }
        consumer.stream.emit(observation, bytes, record.peerId, bytes - RECORD_BYTES)
      }
    }
  }

  private onScanEnd(record: Extract<WireDrainRecord, { t: 'scan-end' }>): void {
    const group = this.scanGroups.get(record.operationId)
    if (group === undefined) {
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'unmatched-scan-end',
        message: 'The owner ended a scan membership this backend does not hold',
        // The membership id is the owner's wire vocabulary; the reason is the fact.
        detail: Object.freeze({ reason: record.reason })
      })
      return
    }
    // The owner released the membership itself: nothing remains to stop.
    this.endScanGroup(group, record.reason, null)
    this.scanGroups.delete(record.operationId)
  }

  // -- connections -------------------------------------------------------------------------------

  /**
   * Cancel this provider's in-flight acquisition for `nativePeerId`, if any,
   * and wait for it to settle (finding 194). The superseded caller already
   * holds its terminal answer — it moved on, which is why a newer connect is
   * here. Cancelling reaps the owner's `Connecting` claim, so the new
   * connect arbitrates against a terminal record. Best-effort: a cancel that
   * cannot land leaves today's arbitration answer in place, and the failure
   * is reported on the background channel, never swallowed. A live
   * established link is untouched: only a pending acquisition is ever
   * superseded. Mirrors the desktop provider's supersede.
   */
  private async supersedePendingAcquisition(nativePeerId: string): Promise<void> {
    const pending = this.pendingAcquisitions.get(nativePeerId)
    if (pending === undefined) return
    this.pendingAcquisitions.delete(nativePeerId)
    try {
      await this.cancel(pending.operationId)
    } catch (error) {
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'connect-supersede-cancel-failed',
        message: 'Superseding a stale connect acquisition did not cancel it',
        detail: Object.freeze({ code: normalizedFrom(error, `${SCOPE}.op.cancel`).code })
      })
      return
    }
    await pending.settled
  }

  private async connect(
    peerId: PeerId<string>,
    _clientId: ClientId<string, string>,
    options: ConnectionOptions
  ): Promise<ConnectionLease<string, string, string>> {
    const operation = `${SCOPE}.connection.connect`
    this.assertOperational(operation)
    const nativePeerId = this.nativeIdForPeerId(String(peerId), operation)
    const intent = options.intent ?? 'direct'
    if (intent === 'when-available' && this.platform === 'apple') {
      // CoreBluetooth has no autoConnect; the capability is not registered.
      throw contractError('capability.unsupported', 'connection', `${operation}.when-available`)
    }
    await this.supersedePendingAcquisition(nativePeerId)
    const operationId = this.mintOperationId('connect')
    const ordinal = this.nextOrdinal
    this.nextOrdinal += 1
    const lease = `lease-${ordinal}`
    // Legacy numbered the connection before the native connect, so a failed
    // connect consumes its number too.
    const connectionOrdinal = this.nextConnection
    this.nextConnection += 1
    const args: WireJsonObject = {
      peerId: nativePeerId,
      lease,
      operationId,
      intent,
      transport: options.transport ?? 'auto',
      preferredPhy: [...(options.preferredPhy ?? [])],
      ...this.budget(options, operation)
    }
    const removeAbort = this.watchAbort(options.signal, operationId, operation)
    let connected: WireOpResults['connection.connect']
    try {
      const acquisition = this.invoke('connection.connect', args)
      this.pendingAcquisitions.set(nativePeerId, {
        operationId,
        settled: acquisition.then(
          () => undefined,
          () => undefined
        )
      })
      try {
        connected = await acquisition
      } finally {
        if (this.pendingAcquisitions.get(nativePeerId)?.operationId === operationId) {
          this.pendingAcquisitions.delete(nativePeerId)
        }
      }
    } finally {
      removeAbort()
    }
    const connectionId = this.identifiers.connectionId(`corebluetooth-connection-${connectionOrdinal}`)
    const leaseId = this.identifiers.leaseId(`corebluetooth-connection-lease-${connectionOrdinal}`)
    const key = String(connectionId)
    const entry: ConnectionEntry = {
      key,
      nativePeerId,
      lease,
      coreGeneration: connected.connectionGeneration,
      linkState: 'connected',
      release: null,
      released: false,
      databases: new Set(),
      resource: Object.freeze({
        attachment: this.attachmentRecord,
        attachmentId: this.attachmentRecord.attachmentId,
        peerId,
        connectionId,
        // The public generation is this backend's legacy name for the
        // owner's `coreGeneration` above, which alone crosses the wire.
        connectionGeneration: opaqueId(
          `corebluetooth-connection-generation-${connectionOrdinal}`,
          'connection-generation',
          SCOPE
        ),
        get state() {
          return entry.released ? 'disconnected' : entry.linkState === 'lost' ? 'lost' : 'connected'
        },
        disconnect: () => this.releaseConnection(entry)
      })
    }
    this.connectionsByKey.set(key, entry)
    this.leaseIds.set(key, leaseId)
    this.invalidations.delete(nativePeerId)
    this.connectionsByLink.set(linkKey(nativePeerId, connected.connectionGeneration), entry)
    await this.refreshCounters()
    return Object.freeze({ leaseId, connection: entry.resource, release: () => this.releaseConnection(entry) })
  }

  /**
   * Releases the core lease. The mapping stays until the owner confirms
   * release; a `release-failed` record keeps it for a retry (PR210-09).
   */
  private releaseConnection(entry: ConnectionEntry): Promise<CleanupRecord> {
    if (entry.released) return Promise.resolve(RELEASED)
    if (entry.release !== null) return entry.release
    const release = (async (): Promise<CleanupRecord> => {
      let record: CleanupRecord
      try {
        record = cleanupRecordFrom(
          await this.invoke('connection.disconnect', { peerId: entry.nativePeerId, lease: entry.lease })
        )
      } catch (error) {
        record = {
          state: 'release-failed',
          failures: [cleanupFailure('connection', error, `${SCOPE}.connection.disconnect`)]
        }
      }
      if (record.state === 'released') {
        entry.released = true
        this.forgetConnection(entry)
        await this.refreshCounters()
      }
      return record
    })()
    entry.release = release
    release.then(
      () => {
        if (entry.release === release) entry.release = null
      },
      () => {
        if (entry.release === release) entry.release = null
      }
    )
    return release
  }

  private forgetConnection(entry: ConnectionEntry): void {
    this.connectionsByKey.delete(entry.key)
    this.leaseIds.delete(entry.key)
    const link = linkKey(entry.nativePeerId, entry.coreGeneration)
    if (this.connectionsByLink.get(link) === entry) {
      this.connectionsByLink.delete(link)
      if (entry.linkState === 'connected') this.rememberRetiredLink(link)
    }
    for (const database of entry.databases) this.databases.delete(database)
    entry.databases.clear()
  }

  private rememberRetiredLink(link: string): void {
    this.retiredLinks.add(link)
    if (this.retiredLinks.size > RETIRED_LINK_CAPACITY) {
      const oldest = this.retiredLinks.values().next().value
      if (oldest !== undefined) this.retiredLinks.delete(oldest)
    }
  }

  private markLost(entry: ConnectionEntry): void {
    entry.linkState = 'lost'
    for (const database of entry.databases) {
      const stored = this.databases.get(database)
      if (stored !== undefined) stored.valid = false
    }
  }

  private connectionPath(entry: ConnectionEntry, leaseId: LeaseId<string, string>) {
    return Object.freeze({
      attachment: this.attachmentRecord,
      attachmentId: this.attachmentRecord.attachmentId,
      peerId: entry.resource.peerId,
      connectionId: entry.resource.connectionId,
      ownerLeaseId: leaseId,
      connectionGeneration: entry.resource.connectionGeneration
    })
  }

  private onLink(
    record: Pick<Extract<WireDrainRecord, { t: 'link' }>, 'peerId' | 'connectionGeneration' | 'reason'>
  ): void {
    const link = linkKey(record.peerId, record.connectionGeneration)
    const entry = this.connectionsByLink.get(link)
    if (entry === undefined) {
      if (this.retiredLinks.delete(link)) return
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'unmatched-link',
        message: 'The owner reported a link transition for a connection this backend does not hold',
        detail: Object.freeze({ reason: record.reason })
      })
      return
    }
    this.endLink(entry, record.reason)
  }

  /**
   * The owner ended `entry`'s link. `null`: the owner no longer reports the
   * link but the record saying why was lost at its full control queue, so it
   * ends as lost and its streams end `connection-lost` here (their own
   * `stream-end` records may have been lost with it).
   */
  private endLink(entry: ConnectionEntry, reason: 'local' | 'peer' | 'adapter' | null): void {
    this.connectionsByLink.delete(linkKey(entry.nativePeerId, entry.coreGeneration))
    // An adapter loss failed the link's streams at their source, as the
    // legacy adapter-loss cleanup (and every desktop host and Tauri) said it.
    const invalidation: InvalidationReason = reason === 'adapter' ? 'source-failed' : 'connection-lost'
    this.invalidations.set(entry.nativePeerId, invalidation)
    this.markLost(entry)
    const path = this.connectionPath(entry, this.leaseIdFor(entry))
    if (reason === null) {
      for (const [consumer, stored] of [...this.subscriptions]) {
        if (stored.connectionKey !== entry.key || stored.state === 'ended') continue
        this.subscriptions.delete(consumer)
        stored.state = 'ended'
        stored.stream.finishWithReason(invalidation)
      }
    }
    if (reason === 'peer' || reason === null) {
      this.emitEvent({ kind: 'connection-lost', connection: path })
    } else if (reason === 'adapter') {
      // Legacy `terminalizeAdapterLossConnection`: connected -> lost, reason adapter.
      this.emitEvent({
        kind: 'connection-state-changed',
        connection: path,
        previous: 'connected',
        current: 'lost',
        reason: 'adapter'
      })
    } else {
      this.emitEvent({ kind: 'disconnected', connection: path, reason })
    }
  }

  private readonly leaseIds = new Map<string, LeaseId<string, string>>()

  private leaseIdFor(entry: ConnectionEntry): LeaseId<string, string> {
    const known = this.leaseIds.get(entry.key)
    if (known === undefined) {
      throw contractError('lifecycle.invariant-violation', 'connection', `${SCOPE}.connection.lease`)
    }
    return known
  }

  /**
   * The owner's `db-changed` names the database generation the change
   * invalidated: every database this backend holds of that generation is
   * stale (the owner is undiscovered until the next discover).
   */
  private onDatabaseChanged(
    record: Pick<
      Extract<WireDrainRecord, { t: 'db-changed' }>,
      'peerId' | 'connectionGeneration' | 'databaseGeneration'
    >
  ): void {
    const entry = this.connectionsByLink.get(linkKey(record.peerId, record.connectionGeneration))
    if (entry === undefined) {
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'unmatched-database-change',
        message: 'The owner reported a database change for a connection this backend does not hold',
        detail: Object.freeze({})
      })
      return
    }
    this.invalidations.set(record.peerId, 'service-changed')
    for (const key of entry.databases) {
      const stored = this.databases.get(key)
      if (stored === undefined || !stored.valid || stored.coreGeneration !== record.databaseGeneration) continue
      stored.valid = false
      this.emitEvent({ kind: 'database-changed', database: stored.path })
    }
  }

  private requireConnection(connection: BackendConnection<string, string>, operation: string): ConnectionEntry {
    this.assertOperational(operation)
    const entry = this.connectionsByKey.get(String(connection.connectionId))
    if (
      entry === undefined ||
      entry.released ||
      entry.linkState !== 'connected' ||
      entry.resource.connectionGeneration !== connection.connectionGeneration
    ) {
      throw contractError('connection.stale', 'connection', operation)
    }
    return entry
  }

  // -- connection controls -----------------------------------------------------------------------

  private control<Result, Operation extends string>(
    connection: BackendConnection<string, string>,
    request: { readonly operation: OperationOptions<string, Operation> },
    name: string,
    run: (entry: ConnectionEntry, operationId: string, budget: { readonly budgetMs?: number }) => Promise<Result>
  ): BackendOperationDispatch<string, Result> {
    const operation = `${SCOPE}.connection.${name}`
    const entry = this.requireConnection(connection, operation)
    const operationId = this.mintOperationId(name)
    const budget = this.budget(request.operation, operation)
    return this.dispatch(operationId, request.operation.signal, operation, () => run(entry, operationId, budget))
  }

  private readRssi<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadRssiRequest<string, Operation>
  ): BackendOperationDispatch<string, RssiMeasurement<string, Operation>> {
    return this.control(connection, request, 'rssi', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.rssi', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        operationId,
        ...budget
      })
      return Object.freeze({
        rssi: answer.rssi,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private requestMtu<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestMtuRequest<string, Operation>
  ): BackendOperationDispatch<string, MtuNegotiation<string, Operation>> {
    return this.control(connection, request, 'request-mtu', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.request-mtu', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        mtu: request.requestedMtu,
        operationId,
        ...budget
      })
      return Object.freeze({
        requestedMtu: request.requestedMtu,
        negotiatedMtu: answer.mtu,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private effectiveMtu<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: EffectiveMtuRequest<string, Operation>
  ): BackendOperationDispatch<string, EffectiveMtuMeasurement<string, Operation>> {
    return this.control(connection, request, 'effective-mtu', async entry => {
      const answer = await this.invoke('connection.effective-mtu', { peerId: entry.nativePeerId, lease: entry.lease })
      return Object.freeze({
        connectionId: entry.resource.connectionId,
        connectionGeneration: entry.resource.connectionGeneration,
        attMtu: answer.mtu,
        payloadBytes: answer.mtu === null ? null : answer.mtu - 3,
        platformPduBytes: null,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private requestPriority<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestPriorityRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPriorityRequest<string, Operation>> {
    return this.control(connection, request, 'request-priority', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.request-priority', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        priority: request.priority,
        operationId,
        ...budget
      })
      return Object.freeze({
        requested: request.priority,
        accepted: answer.accepted,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private readPhy<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadPhyRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPhyObservation<string, Operation>> {
    return this.control(connection, request, 'read-phy', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.read-phy', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        operationId,
        ...budget
      })
      return Object.freeze({
        txPhy: answer.tx,
        rxPhy: answer.rx,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private requestPhy<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestPhyRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPhyRequest<string, Operation>> {
    return this.control(connection, request, 'request-phy', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.request-phy', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        operationId,
        ...(request.preference.tx === undefined ? {} : { tx: request.preference.tx }),
        ...(request.preference.rx === undefined ? {} : { rx: request.preference.rx }),
        ...budget
      })
      const observedAtMonotonicMs = this.now()
      const terminal = this.terminal(request.operation.correlation)
      return Object.freeze({
        requested: Object.freeze({ ...request.preference }),
        accepted: answer.accepted,
        observation:
          answer.observation === null
            ? null
            : Object.freeze({
                txPhy: answer.observation.tx,
                rxPhy: answer.observation.rx,
                observedAtMonotonicMs,
                terminal
              }),
        observedAtMonotonicMs,
        terminal
      })
    })
  }

  /**
   * The platform's own largest single write in `mode` on this link
   * (`connection.maximum-write-length`): CoreBluetooth
   * `maximumWriteValueLength(for:)`; Android 512 with response (the stack's
   * long write) and one ATT payload of the negotiated MTU, or of the ATT
   * default 23 before any exchange, without.
   */
  private maximumWriteLength<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ConnectionMaximumWriteLengthRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionMaximumWriteLengthMeasurement<string, Operation>> {
    return this.control(connection, request, 'maximum-write-length', async (entry, operationId, budget) => {
      const answer = await this.invoke('connection.maximum-write-length', {
        peerId: entry.nativePeerId,
        lease: entry.lease,
        mode: request.mode,
        operationId,
        ...budget
      })
      return Object.freeze({
        connectionId: entry.resource.connectionId,
        connectionGeneration: entry.resource.connectionGeneration,
        mode: request.mode,
        maximumWriteLength: answer.maximumWriteLength,
        observedAtMonotonicMs: this.now(),
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  /** The `gatt:maximum-write-length` registration's answer for one current connection. */
  private async observeMaximumWriteLength(
    input: MaximumWriteLengthFeatureInput
  ): Promise<MaximumWriteLengthFeatureOutput> {
    const operation = `${SCOPE}.gatt.maximum-write-length`
    if (input.mode !== 'with-response' && input.mode !== 'without-response') {
      throw contractError('argument.invalid', 'gatt', operation)
    }
    const entry = this.connectionsByKey.get(input.connectionId)
    if (entry === undefined) throw contractError('connection.not-found', 'connection', operation)
    if (String(entry.resource.connectionGeneration) !== input.connectionGeneration) {
      throw contractError('connection.stale', 'connection', operation)
    }
    const measured = await this.maximumWriteLength(entry.resource, {
      operation: this.operationFor({ signal: null, deadline: null }),
      mode: input.mode
    }).completion
    return Object.freeze({
      connectionId: input.connectionId,
      connectionGeneration: input.connectionGeneration,
      mode: input.mode,
      maximumWriteLength: measured.maximumWriteLength,
      observedAtMonotonicMs: Math.floor(measured.observedAtMonotonicMs)
    })
  }

  // -- GATT ------------------------------------------------------------------------------------------

  private async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    const operation = `${SCOPE}.gatt.discover`
    const entry = this.requireConnection(connection, operation)
    const operationId = this.mintOperationId('discover')
    const args: WireJsonObject = {
      peerId: entry.nativePeerId,
      lease: entry.lease,
      operationId,
      ...this.budget(options, operation)
    }
    const removeAbort = this.watchAbort(options.signal, operationId, operation)
    let discovery: WireDiscovery
    try {
      discovery = await this.invoke('gatt.discover', args)
    } finally {
      removeAbort()
    }
    if (discovery.connectionGeneration !== entry.coreGeneration) {
      throw contractError('protocol.violation', 'gatt', `${operation}.connection-generation`)
    }
    this.requireConnection(connection, operation)
    const ordinal = this.nextDatabase
    this.nextDatabase += 1
    const databaseId = this.identifiers.databaseId(`corebluetooth-database-${ordinal}`)
    const leaseId = this.leaseIdFor(entry)
    const path: DatabasePath<string, string, string> = Object.freeze({
      ...this.connectionPath(entry, leaseId),
      databaseId,
      // Legacy name for the owner's `coreGeneration` below.
      databaseGeneration: opaqueId(`corebluetooth-database-generation-${ordinal}`, 'database-generation', SCOPE)
    })
    const key = String(databaseId)
    const stored: DatabaseEntry = {
      key,
      connectionKey: entry.key,
      coreGeneration: discovery.databaseGeneration,
      path,
      discovery,
      valid: true
    }
    for (const previous of entry.databases) {
      const old = this.databases.get(previous)
      if (old !== undefined) old.valid = false
    }
    this.databases.set(key, stored)
    entry.databases.add(key)
    await this.refreshCounters()
    return Object.freeze({
      path,
      snapshot: async () => this.snapshot(stored),
      read: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ): Promise<CharacteristicRead> => {
        const { value, provenance } = await this.read(characteristic, {
          operation: this.operationFor(readOptions)
        }).completion
        return Object.freeze({ value, provenance })
      },
      write: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        value: BorrowedBytes,
        writeOptions: WritePolicy
      ): Promise<WriteReceipt<string, string>> =>
        this.write(characteristic, {
          operation: this.operationFor(writeOptions),
          bytes: value,
          mode: writeOptions.mode
        }).completion,
      readDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ) => (await this.readDescriptor(descriptor, { operation: this.operationFor(readOptions) }).completion).value,
      writeDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        value: BorrowedBytes,
        writeOptions: WritePolicy
      ): Promise<WriteReceipt<string, string>> =>
        this.writeDescriptor(descriptor, {
          operation: this.operationFor(writeOptions),
          bytes: value,
          mode: writeOptions.mode
        }).completion,
      subscribe: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        subscribeOptions: SubscriptionOptions
      ): Promise<Subscription<string, string, string, string, string, string>> => {
        const subscription = await this.subscribe(characteristic, {
          operation: this.operationFor(subscribeOptions),
          options: subscribeOptions
        }).completion
        return Object.freeze({
          subscriptionId: subscription.subscriptionId,
          path: subscription.path,
          values: subscription.notifications,
          remove: async (): Promise<CleanupRecord> => {
            try {
              await this.unsubscribe(subscription, {
                signal: null,
                deadline: null,
                // Legacy ran no unsubscribe through the core's counter; its
                // terminal is never public.
                correlation: this.identifiers.operationCorrelation(this.mintOperationId('unsubscribe'))
              }).completion
              return RELEASED
            } catch (error) {
              return {
                state: 'release-failed',
                failures: [cleanupFailure('subscription', error, `${SCOPE}.gatt.unsubscribe`)]
              }
            }
          }
        })
      }
    })
  }

  private operationFor(options: PublicOperationOptions): OperationOptions<string, string> {
    return Object.freeze({
      signal: options.signal,
      deadline: options.deadline,
      correlation: this.publicCorrelation()
    })
  }

  private snapshot(stored: DatabaseEntry): GattDatabaseSnapshot<string, string, string> {
    this.assertOperational(`${SCOPE}.gatt.snapshot`)
    if (!stored.valid) throw contractError('gatt.stale-handle', 'gatt', `${SCOPE}.gatt.snapshot`)
    const services: Service<string, string, string, string>[] = []
    const characteristics: Characteristic<string, string, string, string, string>[] = []
    const descriptors: Descriptor<string, string, string, string, string, string>[] = []
    for (const service of stored.discovery.services) {
      const servicePath = Object.freeze({
        ...stored.path,
        serviceUuid: canonicalUuid(service.uuid),
        serviceOccurrence: opaqueId(String(service.occurrence), 'service-occurrence', SCOPE)
      })
      services.push(Object.freeze({ path: servicePath, primary: true, includedServices: Object.freeze([]) }))
      for (const characteristic of service.characteristics) {
        const characteristicPath = Object.freeze({
          ...servicePath,
          characteristicUuid: canonicalUuid(characteristic.uuid),
          characteristicOccurrence: opaqueId(String(characteristic.occurrence), 'characteristic-occurrence', SCOPE),
          validity: 'current' as const
        })
        characteristics.push(
          Object.freeze({
            path: characteristicPath,
            properties: characteristicPropertiesFromBits(characteristic.properties),
            access: Object.freeze({ read: 'unknown' as const, write: 'unknown' as const })
          })
        )
        for (const descriptor of characteristic.descriptors) {
          descriptors.push(
            Object.freeze({
              path: Object.freeze({
                ...characteristicPath,
                descriptorUuid: canonicalUuid(descriptor.uuid),
                descriptorOccurrence: opaqueId(String(descriptor.occurrence), 'descriptor-occurrence', SCOPE)
              }),
              properties: createGattDescriptorProperties(
                true,
                true,
                { read: 'unknown', write: 'unknown' },
                { read: 'unknown', write: 'unknown' }
              )
            })
          )
        }
      }
    }
    return Object.freeze({
      path: stored.path,
      services: Object.freeze(services),
      characteristics: Object.freeze(characteristics),
      descriptors: Object.freeze(descriptors)
    })
  }

  /**
   * Resolves a handle against the discovery that issued it: the database and
   * its connection must be the core's current ones, and the attribute must
   * exist. Anything else fails before native I/O (PR210-16).
   */
  private resolveCharacteristic(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): { readonly entry: ConnectionEntry; readonly database: DatabaseEntry; readonly selector: CharacteristicSelector } {
    this.assertOperational(operation)
    if (path.validity !== 'current') throw contractError('gatt.stale-handle', 'gatt', operation)
    const database = this.databases.get(String(path.databaseId))
    if (database === undefined || !database.valid || database.path.databaseGeneration !== path.databaseGeneration) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    const entry = this.connectionsByKey.get(database.connectionKey)
    if (
      entry === undefined ||
      entry.linkState !== 'connected' ||
      String(path.connectionId) !== database.connectionKey ||
      path.connectionGeneration !== entry.resource.connectionGeneration
    ) {
      throw contractError('connection.stale', 'connection', operation)
    }
    const serviceUuid = String(path.serviceUuid)
    const characteristicUuid = String(path.characteristicUuid)
    for (const service of database.discovery.services) {
      if (service.uuid !== serviceUuid || String(service.occurrence) !== String(path.serviceOccurrence)) continue
      for (const characteristic of service.characteristics) {
        if (
          characteristic.uuid === characteristicUuid &&
          String(characteristic.occurrence) === String(path.characteristicOccurrence)
        ) {
          return {
            entry,
            database,
            selector: Object.freeze({
              serviceUuid: service.uuid,
              serviceOccurrence: service.occurrence,
              characteristicUuid: characteristic.uuid,
              characteristicOccurrence: characteristic.occurrence
            })
          }
        }
      }
    }
    throw contractError('gatt.not-found', 'gatt', operation)
  }

  private resolveDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): { readonly entry: ConnectionEntry; readonly selector: DescriptorSelector } {
    const resolved = this.resolveCharacteristic(path, operation)
    for (const service of resolved.database.discovery.services) {
      if (service.uuid !== resolved.selector.serviceUuid || service.occurrence !== resolved.selector.serviceOccurrence)
        continue
      for (const characteristic of service.characteristics) {
        if (
          characteristic.uuid !== resolved.selector.characteristicUuid ||
          characteristic.occurrence !== resolved.selector.characteristicOccurrence
        ) {
          continue
        }
        const descriptor = characteristic.descriptors.find(
          candidate =>
            candidate.uuid === String(path.descriptorUuid) &&
            String(candidate.occurrence) === String(path.descriptorOccurrence)
        )
        if (descriptor !== undefined) {
          return {
            entry: resolved.entry,
            selector: Object.freeze({
              ...resolved.selector,
              descriptorUuid: descriptor.uuid,
              descriptorOccurrence: descriptor.occurrence
            })
          }
        }
      }
    }
    throw contractError('gatt.not-found', 'gatt', operation)
  }

  private read<Operation extends string>(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, Operation>
  ): BackendOperationDispatch<string, CharacteristicReadResult<string, Operation>> {
    const operation = `${SCOPE}.gatt.read`
    const { entry, selector } = this.resolveCharacteristic(path, operation)
    const operationId = this.mintOperationId('read')
    const budget = this.budget(request.operation, operation)
    return this.dispatch(operationId, request.operation.signal, operation, async () => {
      const answer = await this.invoke('gatt.read', { peerId: entry.nativePeerId, selector, operationId, ...budget })
      return Object.freeze({
        value: ownedCopy(answer.value),
        provenance: answer.provenance,
        terminal: this.terminal(request.operation.correlation)
      })
    })
  }

  private readDescriptor<Operation extends string>(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, Operation>
  ): BackendOperationDispatch<string, ReadResult<string, Operation>> {
    const operation = `${SCOPE}.gatt.read-descriptor`
    const { entry, selector } = this.resolveDescriptor(path, operation)
    const operationId = this.mintOperationId('read-descriptor')
    const budget = this.budget(request.operation, operation)
    return this.dispatch(operationId, request.operation.signal, operation, async () => {
      const answer = await this.invoke('gatt.read-descriptor', {
        peerId: entry.nativePeerId,
        selector,
        operationId,
        ...budget
      })
      return Object.freeze({ value: ownedCopy(answer.value), terminal: this.terminal(request.operation.correlation) })
    })
  }

  /** A write's result is the owner's receipt; its failure carries the owner's commit state. */
  private writeWith<Operation extends string>(
    op: 'gatt.write' | 'gatt.write-descriptor',
    nativePeerId: string,
    selector: CharacteristicSelector | DescriptorSelector,
    request: WriteRequest<string, Operation>,
    operation: string
  ): BackendOperationDispatch<string, WriteResult<string, Operation>> {
    if (!(request.bytes instanceof Uint8Array)) throw contractError('argument.invalid', 'gatt', operation)
    const valueB64 = unwrap(encodeBase64(request.bytes))
    const operationId = this.mintOperationId('write')
    const budget = this.budget(request.operation, operation)
    return this.dispatch(operationId, request.operation.signal, operation, async () => {
      const receipt = unwrap(
        checkWriteReceipt(
          await this.invoke(op, {
            peerId: nativePeerId,
            selector,
            valueB64,
            mode: request.mode,
            operationId,
            ...budget
          }),
          request.mode
        )
      )
      return Object.freeze({ terminal: this.terminal(request.operation.correlation), commitState: receipt.commitState })
    })
  }

  private write<Operation extends string>(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, Operation>
  ): BackendOperationDispatch<string, WriteResult<string, Operation>> {
    const operation = `${SCOPE}.gatt.write`
    const { entry, selector } = this.resolveCharacteristic(path, operation)
    return this.writeWith('gatt.write', entry.nativePeerId, selector, request, operation)
  }

  private writeDescriptor<Operation extends string>(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, Operation>
  ): BackendOperationDispatch<string, WriteResult<string, Operation>> {
    const operation = `${SCOPE}.gatt.write-descriptor`
    const { entry, selector } = this.resolveDescriptor(path, operation)
    return this.writeWith('gatt.write-descriptor', entry.nativePeerId, selector, request, operation)
  }

  private subscribe<Operation extends string>(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, Operation>
  ): BackendOperationDispatch<string, BackendSubscription<string, string, string, string, string>> {
    const operation = `${SCOPE}.gatt.subscribe`
    const { entry, selector } = this.resolveCharacteristic(path, operation)
    const operationId = this.mintOperationId('subscribe')
    const budget = this.budget(request.operation, operation)
    const consumer = this.mintOperationId('consumer')
    const subscriptionId = this.identifiers.subscriptionId(`corebluetooth-subscription-${this.nextSubscription}`)
    this.nextSubscription += 1
    // Registered before the owner can deliver: a value that arrives in the
    // drain before `gatt.subscribe` resolves is routed, not lost.
    const stream: OwnedCoreBoundedStream<NotificationValue> = new OwnedCoreBoundedStream<NotificationValue>(
      request.options.delivery,
      request.options.delivery.overflowPolicy,
      () => undefined
    )
    const stored: SubscriptionEntry = {
      consumer,
      subscriptionId,
      nativePeerId: entry.nativePeerId,
      connectionKey: entry.key,
      selector,
      stream,
      state: 'subscribing',
      removal: null,
      ingressDropped: 0
    }
    this.subscriptions.set(consumer, stored)
    return this.dispatch(operationId, request.operation.signal, operation, async () => {
      try {
        await this.invoke('gatt.subscribe', {
          peerId: entry.nativePeerId,
          selector,
          consumer,
          operationId,
          ...(request.options.deliveryMode === undefined ? {} : { deliveryMode: request.options.deliveryMode }),
          ...budget
        })
      } catch (error) {
        this.subscriptions.delete(consumer)
        stream.closeWithReason('source-failed', normalizedFrom(error, operation))
        throw error
      }
      if (stored.state === 'subscribing') stored.state = 'active'
      await this.refreshCounters()
      return Object.freeze({
        subscriptionId,
        path,
        terminal: this.terminal(request.operation.correlation),
        notifications: stream
      })
    })
  }

  /**
   * Releases one consumer. The consumer and its stream stay registered until
   * the owner confirms; a failure keeps them for a retry with the same
   * identity (PR210-09). A consumer the owner already retired
   * (`stream-end`) needs no native call.
   */
  private unsubscribe<Operation extends string>(
    subscription: BackendSubscription<string, string, string, string, string>,
    operationOptions: OperationOptions<string, Operation>
  ): BackendOperationDispatch<string, OperationTerminalRecord<string, string>> {
    const operation = `${SCOPE}.gatt.unsubscribe`
    this.assertOperational(operation)
    const stored = [...this.subscriptions.values()].find(entry => entry.subscriptionId === subscription.subscriptionId)
    const operationId = this.mintOperationId('unsubscribe')
    if (stored === undefined || stored.state === 'ended') {
      if (stored !== undefined) this.subscriptions.delete(stored.consumer)
      return this.dispatch(operationId, null, operation, async () => this.terminal(operationOptions.correlation))
    }
    if (stored.removal !== null) {
      const removal = stored.removal
      return this.dispatch(operationId, null, operation, () => removal)
    }
    const budget = this.budget(operationOptions, operation)
    const dispatch = this.dispatch(operationId, operationOptions.signal, operation, async () => {
      try {
        await this.invoke('gatt.unsubscribe', {
          peerId: stored.nativePeerId,
          selector: stored.selector,
          consumer: stored.consumer,
          operationId,
          ...budget
        })
      } finally {
        stored.removal = null
      }
      this.subscriptions.delete(stored.consumer)
      stored.state = 'ended'
      stored.stream.closeWithReason('owner-released')
      await this.refreshCounters()
      return this.terminal(operationOptions.correlation)
    })
    stored.removal = dispatch.completion
    return dispatch
  }

  private onValue(record: Extract<WireDrainRecord, { t: 'value' }>): void {
    const stored = this.subscriptions.get(record.consumer)
    if (stored === undefined || stored.state === 'ended') {
      this.emitEvent({
        kind: 'diagnostic-warning',
        code: 'unmatched-notification',
        message: 'The owner delivered a value for a consumer this backend does not hold',
        detail: Object.freeze({ bytes: record.value.byteLength })
      })
      return
    }
    const value: NotificationValue = Object.freeze({ value: ownedCopy(record.value), delivery: record.delivery })
    stored.stream.emit(value, record.value.byteLength)
  }

  private onStreamEnd(
    record: Pick<Extract<WireDrainRecord, { t: 'stream-end' }>, 'consumer' | 'reason' | 'droppedItems' | 'droppedBytes'>
  ): void {
    const stored = this.subscriptions.get(record.consumer)
    if (stored === undefined) return
    this.subscriptions.delete(record.consumer)
    stored.state = 'ended'
    if (record.droppedItems > 0 || record.droppedBytes > 0) {
      stored.stream.observeSourceOverflow({
        kind: 'overflow',
        policy: 'error',
        droppedItems: resourceCount(record.droppedItems + stored.ingressDropped),
        droppedBytes: resourceCount(record.droppedBytes),
        replacedItems: resourceCount(0)
      })
    }
    const reason: CoreStreamTerminalReason =
      record.reason === 'overflow'
        ? 'overflow'
        : record.reason === 'closed'
          ? 'closed'
          : (this.invalidations.get(stored.nativePeerId) ?? 'connection-lost')
    stored.stream.finishWithReason(reason)
  }

  // -- drain ------------------------------------------------------------------------------------------

  private deliver(record: WireDrainRecord): void {
    switch (record.t) {
      case 'adv':
        this.onAdvertisement(record)
        break
      case 'scan-end':
        this.onScanEnd(record)
        break
      case 'value':
        this.onValue(record)
        break
      case 'stream-end':
        this.onStreamEnd(record)
        break
      case 'adapter':
        this.onAdapterRecord(record.state)
        break
      case 'link':
        this.onLink(record)
        break
      case 'db-changed':
        this.onDatabaseChanged(record)
        break
      case 'ingress-drop':
        this.onIngressDrop(record)
        break
      case 'security':
        this.onSecurity(record.peerId, record.state)
        break
      case 'restored':
        this.onRestored(record.peers)
        break
    }
  }

  /**
   * The owner dropped records at a full native queue. A dropped advertisement
   * or notification is lost before routing, so the owner cannot say which
   * consumer it was for: every stream that could have received it counts it
   * in its drop accounting (an upper bound, never silence). A dropped control
   * record is re-read from the owner.
   */
  /** The last control-loss total acted on (X-R5; monotonic per session). */
  private lastControlLostTotal = 0

  /**
   * The owner's cumulative control-loss counter moved (X-R5): control
   * records were refused behind queued data, so the gap is reported
   * promptly instead of waiting for the queues to empty. The in-band
   * `ingress-drop` record for the same loss still arrives later and
   * reconciles again (one re-read runs at a time); both warnings describe
   * the same class of fact and neither is inferred.
   */
  private noteControlLoss(total: number): void {
    if (total <= this.lastControlLostTotal) return
    const count = total - this.lastControlLostTotal
    this.lastControlLostTotal = total
    this.reconcileControlLoss()
    this.emitEvent({
      kind: 'diagnostic-warning',
      code: 'native-ingress-drop',
      message: `The Rust owner dropped ${count} control record(s) at a full queue`,
      detail: Object.freeze({ class: 'control', count })
    })
  }

  private onIngressDrop(record: Extract<WireDrainRecord, { t: 'ingress-drop' }>): void {
    if (record.class === 'advertisement') {
      for (const group of this.scanGroups.values()) {
        for (const consumer of group.consumers.values()) this.noteIngressLoss(consumer, record.count)
      }
    } else if (record.class === 'notification') {
      for (const stored of this.subscriptions.values()) {
        if (stored.state !== 'ended') this.noteIngressLoss(stored, record.count)
      }
    } else {
      this.reconcileControlLoss()
    }
    this.emitEvent({
      kind: 'diagnostic-warning',
      code: 'native-ingress-drop',
      message: `The Rust owner dropped ${record.count} ${record.class} record(s) at a full queue`,
      detail: Object.freeze({ class: record.class, count: record.count })
    })
  }

  private noteIngressLoss(account: IngressLossAccount, count: number): void {
    account.ingressDropped += count
    account.stream.observeSourceOverflow({
      kind: 'overflow',
      policy: 'drop-newest',
      droppedItems: resourceCount(account.ingressDropped),
      droppedBytes: resourceCount(0),
      replacedItems: resourceCount(0)
    })
  }

  private controlReconcile: Promise<void> | null = null
  private controlReconcileAgain = false

  /**
   * Control records were lost at the owner's full control queue. The owner
   * answers every fact they carried (`session.reconcile`, 104/105), and each
   * lost record becomes the transition it would have caused: the adapter
   * state; a held link the owner ended (with its reason) or no longer holds
   * under this generation (lost, e.g. it reconnected under a new one); a
   * database change on a held link; a stream the owner ended (its reason and
   * drop counts); a security report or restored set this backend has not
   * delivered; a scan membership the owner no longer holds (`source-failed`).
   * What the owner still reports live is left alone: nothing is inferred.
   * One re-read runs at a time; drops during it schedule one more.
   */
  private reconcileControlLoss(): void {
    if (this.controlReconcile !== null) {
      this.controlReconcileAgain = true
      return
    }
    this.controlReconcile = this.rereadAfterControlLoss()
      .catch((error: unknown) => {
        this.emitEvent({
          kind: 'diagnostic-warning',
          code: 'control-reconcile-failed',
          message: 'The owner did not answer the re-read after a lost control record',
          detail: Object.freeze({ code: normalizedFrom(error, `${SCOPE}.control-reconcile`).code })
        })
      })
      .finally(() => {
        this.controlReconcile = null
        if (this.controlReconcileAgain && !this.destroyed) {
          this.controlReconcileAgain = false
          this.reconcileControlLoss()
        }
      })
  }

  private async rereadAfterControlLoss(): Promise<void> {
    if (this.destroyed) return
    const snapshot = await this.invoke('session.reconcile', {})
    if (this.destroyed) return
    this.onAdapterRecord(snapshot.adapter)
    for (const entry of [...this.connectionsByLink.values()]) {
      if (entry.linkState !== 'connected') continue
      const link = snapshot.links.find(
        candidate => candidate.peerId === entry.nativePeerId && candidate.connectionGeneration === entry.coreGeneration
      )
      if (link === undefined) {
        this.endLink(entry, null)
      } else if (link.state === 'ended') {
        this.onLink(link)
      } else if (link.databaseChange !== null) {
        this.onDatabaseChanged({ ...link, databaseGeneration: link.databaseChange })
      }
    }
    for (const [consumer, stored] of [...this.subscriptions]) {
      if (stored.state !== 'active') continue
      const owned = snapshot.subscriptions.find(candidate => candidate.consumer === consumer)
      if (owned === undefined) {
        this.emitEvent({
          kind: 'diagnostic-warning',
          code: 'reconcile-unknown-consumer',
          message: 'The owner no longer reports a consumer this backend holds active',
          detail: Object.freeze({})
        })
      } else if (owned.state === 'ended') {
        this.onStreamEnd(owned)
      }
    }
    for (const report of snapshot.security) {
      if (this.securityDelivered.get(report.peerId) !== securityKey(report.state)) {
        this.onSecurity(report.peerId, report.state)
      }
    }
    if (snapshot.restored.length > 0) this.onRestored(snapshot.restored)
    for (const [membership, group] of [...this.scanGroups]) {
      if (group.state !== 'active' || membership === snapshot.scan) continue
      this.endScanGroup(group, 'source-failed', null)
      this.scanGroups.delete(membership)
    }
  }

  /** The last security state delivered per native peer (reconcile delivers only a change). */
  private readonly securityDelivered = new Map<string, string>()
  private restoredDelivered: string | null = null

  private onRestored(peers: readonly WireRestoredPeer[]): void {
    const key = JSON.stringify(peers.map(peer => [peer.peerId, peer.name, peer.connected]))
    if (key === this.restoredDelivered) return
    this.restoredDelivered = key
    this.emitEvent({
      kind: 'restoration-received',
      record: Object.freeze({
        peers: Object.freeze(
          peers.map(peer =>
            Object.freeze({ peerId: String(this.peerIdForNative(peer.peerId)), connected: peer.connected })
          )
        )
      })
    })
  }

  private onSecurity(nativePeerId: string, state: WireSecurityState): void {
    this.securityDelivered.set(nativePeerId, securityKey(state))
    const peerId = this.peerIdForNative(nativePeerId)
    this.security?.observe(String(peerId), state)
    this.emitEvent({
      kind: 'bond-security-changed',
      peerId,
      bond:
        state.bond === 'bonded'
          ? 'bonded'
          : state.bond === 'bonding'
            ? 'bonding'
            : state.bond === 'not-bonded'
              ? 'none'
              : 'unavailable',
      security:
        state.encryption === 'encrypted'
          ? state.authentication === 'authenticated'
            ? 'authenticated'
            : 'encrypted'
          : state.encryption === 'not-encrypted'
            ? 'unencrypted'
            : 'unavailable'
    })
  }

  // -- host services ------------------------------------------------------------------------------------

  private async acquireBackground(request: {
    readonly kind: 'connected-device'
    readonly reason: string
  }): Promise<{ readonly leaseId: string }> {
    this.assertOperational(`${SCOPE}.background.acquire`)
    const answer = await this.invoke('background.acquire', {
      kind: request.kind,
      reason: request.reason,
      operationId: this.mintOperationId('background')
    })
    this.backgroundLeases.add(answer.leaseId)
    return answer
  }

  private async releaseBackground(leaseId: string): Promise<CleanupRecord> {
    if (this.destroyed && this.releaseModuleBackground !== null) {
      // The lease outlived this manager (87/N8); the module still holds it.
      return this.releaseModuleBackground(leaseId)
    }
    this.assertOperational(`${SCOPE}.background.release`)
    const record = cleanupRecordFrom(await this.invoke('background.release', { leaseId }))
    if (record.state === 'released') this.backgroundLeases.delete(leaseId)
    return record
  }

  private async updateBackgroundNotification(request: {
    readonly leaseId: string
    readonly title: string
    readonly body?: string
  }): Promise<void> {
    this.assertOperational(`${SCOPE}.background.update-notification`)
    await this.invoke('background.update-notification', {
      leaseId: request.leaseId,
      title: request.title,
      ...(request.body === undefined ? {} : { body: request.body })
    })
  }

  private associateCompanion(request: {
    readonly name?: string
    readonly serviceUuid?: string
  }): Promise<WireOpResults['companion.associate']> {
    this.assertOperational(`${SCOPE}.companion.associate`)
    return this.invoke('companion.associate', {
      ...(request.name === undefined ? {} : { name: request.name }),
      ...(request.serviceUuid === undefined ? {} : { serviceUuid: String(canonicalUuid(request.serviceUuid)) }),
      operationId: this.mintOperationId('companion')
    })
  }

  private observePresence(request: { readonly peerId: string }): Promise<WireOpResults['presence.observe']> {
    this.assertOperational(`${SCOPE}.presence.observe`)
    return this.invoke('presence.observe', {
      peerId: String(request.peerId),
      operationId: this.mintOperationId('presence')
    })
  }

  private unobservePresence(request: { readonly peerId: string }): Promise<WireOpResults['presence.unobserve']> {
    this.assertOperational(`${SCOPE}.presence.unobserve`)
    return this.invoke('presence.unobserve', {
      peerId: String(request.peerId),
      operationId: this.mintOperationId('presence')
    })
  }
}

/** Session services the Expo layer reaches through a Rust-core manager. */
export interface ReactNativeRustCoreHostServices {
  acquireBackground(request: { readonly kind: 'connected-device'; readonly reason: string }): Promise<{
    readonly leaseId: string
  }>
  releaseBackground(leaseId: string): Promise<CleanupRecord>
  updateBackgroundNotification(request: {
    readonly leaseId: string
    readonly title: string
    readonly body?: string
  }): Promise<void>
  associateCompanion(request: { readonly name?: string; readonly serviceUuid?: string }): Promise<{
    readonly source: 'associated'
    readonly associationId: number
    readonly peerId: string | null
    readonly displayName: string | null
  }>
  observePresence(request: { readonly peerId: string }): Promise<{ readonly state: 'observing' }>
  unobservePresence(request: { readonly peerId: string }): Promise<{ readonly state: 'idle' }>
  counters(): Promise<WireCounters>
}

type DistributiveOmit<Type, Key extends PropertyKey> = Type extends unknown ? Omit<Type, Key> : never

/** JSON replacer for event byte accounting: bytes count as their length. */
function jsonSafe(_key: string, value: unknown): unknown {
  if (value instanceof Uint8Array) return value.byteLength
  return value
}

function ownedCopy(bytes: Readonly<Uint8Array>): OwnedBytes {
  return Uint8Array.from(bytes) as OwnedBytes
}

/** Actual retained bytes of one advertisement (payloads, strings, UUID text). */
/** The legacy coordinator's terminal outcome vocabulary for a failed operation. */
function traceOutcomeFor(code: NormalizedBleError['code']): OperationTerminalOutcome {
  switch (code) {
    case 'operation.aborted':
      return 'aborted'
    case 'operation.timed-out':
      return 'timed-out'
    case 'operation.disconnected':
      return 'disconnected'
    case 'lifecycle.destroyed':
      return 'destroyed'
    case 'adapter.unavailable':
    case 'adapter.powered-off':
    case 'adapter.resetting':
      return 'adapter-unavailable'
    default:
      return 'failed'
  }
}

function advertisementBytes(record: Extract<WireDrainRecord, { t: 'adv' }>): number {
  let bytes = RECORD_BYTES + utf8Length(record.peerId)
  if (record.localName !== null) bytes += utf8Length(record.localName)
  for (const list of [record.serviceUuids, record.solicitedServiceUuids, record.overflowServiceUuids]) {
    if (list !== null) bytes += list.length * UUID_BYTES
  }
  for (const entry of record.serviceData ?? []) bytes += UUID_BYTES + entry.payload.byteLength
  for (const entry of record.manufacturerData ?? []) bytes += 2 + entry.payload.byteLength
  if (record.rawRecord !== null) bytes += record.rawRecord.byteLength
  return bytes
}

/** Identity of the connection records for tests that inspect retained state. */
export type { SerializableRecord, GenerationId, WireDelivery }
