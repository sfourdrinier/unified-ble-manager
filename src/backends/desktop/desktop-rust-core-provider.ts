// src/backends/desktop/desktop-rust-core-provider.ts
import { isCanonicalIngressOrdinal } from '../../core/ordered-notification-contract'
import { createHash } from 'node:crypto'
import { encodeTerminalEvidence } from '../../core/terminal-evidence-codec'
//
// The one desktop provider over the shared Rust core (PR210-02): BlueZ,
// CoreBluetooth and WinRT hosts all execute `DesktopCentral` (btleplug plus
// narrow OS adapters, `crates/ubm-desktop`) through the N-API `UbmCentral`
// dispatch (`bindings/napi/src/dispatch.rs`). The platform only selects a
// frozen profile: identity, compatibility, the OS the process must run on,
// and which legacy capability rows the host owes (see
// `desktop-rust-core-parity.ts`).
//
// What executes where:
//   * Rust: admission, deadlines (the caller budget crosses as relative
//     `timeoutMs`; without one the core's named liveness backstops apply),
//     cancellation by ticket, scan ownership, GATT operations, CCCD sharing,
//     connection lifecycle, adapter power, commit/retryability of failures.
//   * TypeScript: branded identities, the scan plan (service UUIDs pushed to
//     the OS filter, the residual matched by the canonical software
//     predicate, exactly as the legacy backends did), scan share/join
//     fan-out over one core scan, the `require-*` delivery property check
//     (FIX-PLAN decision 3), and mapping core events onto contract events.
//
// A missing, unloadable or mismatched core fails loudly before any radio
// call (`src/desktop-core-addon.ts`); there is no TypeScript fallback. No
// public factory or export reaches the legacy CoreBluetooth / WinRT /
// dbus-next BlueZ backends; their sources remain only as parity references
// until they are deleted.

import { BackendContractError, contractError } from '../../backend-contract/errors'
import { snapshotSerializableRecord } from '../../backend-contract/serializable'
import { admitBluezConnectionPolicy, type BluezConnectionPolicy } from './bluez-connection-policy'
import { DesktopOrderedStream, parseOrderedStatus } from './desktop-ordered-group'
import { desktopMarkerReservations } from './desktop-ordered-marker'
import {
  createOrderedRecordingReclaimReservations,
  type OrderedRecordingRecoveryHandle,
  type OrderedRecordingReclaimReceipt
} from '../../core/ordered-recording-reclaim'
import { parseOrderedRecordingStartResult } from '../../core/ordered-recording-start'
import { assertDesktopOrderedReceiptVersion } from './ordered-recording-version'
import {
  isRecordingId,
  MIN_RECORDING_BYTES,
  MAX_RECORDING_BYTES,
  MIN_RECORDING_RECORDS,
  MAX_RECORDING_RECORDS
} from '../../backend-contract/continuation-recording-bounds'
import {
  createNativeContinuationController,
  createNativeContinuationControlAccess
} from './native-continuation-controller'
import {
  compensateDesktopInitialization,
  cleanupDesktopAllocation,
  DesktopProcessHostInitializationError
} from '../../desktop-process-initialization'
import type { CleanupRecord, NormalizedBleError, PlatformErrorDetail } from '../../backend-contract/errors'
import type {
  TerminalEvidenceAppendResult,
  TerminalEvidenceCommit,
  TerminalEvidenceRecord,
  TerminalEvidenceReservation,
  TerminalEvidenceReservationRequest
} from '../../backend-contract/terminal-evidence'
import type {
  BackendAttachment,
  BackendAttachmentRequest,
  BackendEvent,
  BackendConnectionCleanupRecord
} from '../../backend-contract/backend'
import type {
  AdapterBackend,
  BackendConnection,
  BackendPeerQuery,
  BackendSubscription,
  BleCentralBackend,
  ConnectionBackend,
  ConnectionLease,
  ConnectionOptions,
  GattBackend,
  PeerDirectoryBackend,
  ResourceCounters,
  ScanLease,
  ScannerBackend
} from '../../backend-contract/backend'
import type {
  AdapterDescriptor,
  AdapterStateSnapshot,
  AdapterStateWatch,
  AttachmentRecord,
  BackendProvider,
  HostNeutralBackendIdentity
} from '../../backend-contract/identity'
import type { AdapterSelection } from '../../backend-contract/identity'
import {
  advertisementMatchesFilter,
  assertScanFilter,
  decodeWinRtScanPlatformOptions,
  type AdvertisementField,
  type AdvertisementObservation,
  type OwnerScanOptions,
  type ScanFilter,
  type SourceTimestamp
} from '../../backend-contract/advertisement'
import { ScanEvidenceSession } from '../../backend-contract/scan-evidence'
import {
  byteLimit,
  canonicalUuid,
  capacity,
  monotonicTimestamp,
  ownBytes,
  createAttachmentBoundIdFactory,
  negotiateCoreVersions,
  opaqueId,
  resourceCount,
  type AttachmentBoundIdFactory,
  type BackendCompatibilityOffer,
  type ClientId,
  type ConnectionId,
  type GenerationId,
  type LeaseId,
  type OperationCorrelation,
  type OwnedBytes,
  type PeerId,
  type ScanSessionId,
  type ScanShareToken,
  type SerializableRecord,
  type SubscriptionId,
  type Uuid
} from '../../backend-contract/primitives'
import {
  createGattCharacteristicProperties,
  serviceAccessRestriction,
  isNotificationReceipt,
  isNotificationProvenance
} from '../../backend-contract/gatt'
import type {
  CharacteristicPath,
  ConnectionPath,
  DatabasePath,
  DescriptorPath,
  GattDatabase,
  GattDatabaseSnapshot,
  NotificationValue,
  AcquiredGattWriter,
  AcquiredGattNotifications
} from '../../backend-contract/gatt'
import { createBackendOperationDispatch, isReadProvenance } from '../../backend-contract/operations'
import type {
  BackendOperationDispatch,
  CancellationAcknowledgement,
  OperationOptions,
  OperationTerminalRecord,
  PublicOperationOptions,
  CharacteristicRead,
  CharacteristicReadResult,
  ReadProvenance,
  ReadRequest,
  ReadResult,
  SubscribeRequest,
  SubscriptionOptions,
  WriteReceipt,
  WriteRequest,
  WriteResult
} from '../../backend-contract/operations'
import type {
  ConnectionMaximumWriteLengthMeasurement,
  ConnectionMaximumWriteLengthRequest,
  ConnectionParametersMeasurement,
  ConnectionParametersRequest,
  ConnectionParametersStreamObservation,
  ConnectionParametersWatch,
  ConnectionPriorityRequest,
  ConnectionPhyObservation,
  ReadPhyRequest,
  RequestPriorityRequest,
  ConnectionWriteReadinessObservation,
  ConnectionWriteReadinessWatch,
  EffectiveMtuMeasurement,
  EffectiveMtuRequest,
  ReadRssiRequest,
  RssiMeasurement
} from '../../backend-contract/connection-controls'
import { MAXIMUM_REQUESTED_ATT_MTU, MINIMUM_ATT_MTU } from '../../backend-contract/connection-controls'
import { assertConnectionParameterValues } from '../../backend-contract/connection-parameter-validation'
import type { PeerAddressDescriptor } from '../../backend-contract/backend'
import type {
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityBackend,
  SecurityCancelPairingResult,
  SecurityPairOptions,
  SecurityPairResult,
  SecurityUnpairResult
} from '../../backend-contract/security'
import type { BoundedAsyncStream, OverflowPolicy } from '../../backend-contract/streams'
import type { BackendScanExecutionPlan, ScanObservationField, ScanPlan } from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'
import { CoreBoundedStream } from '../../core/bounded-stream'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry,
  type FeatureRegistry,
  type MaximumWriteLengthFeatureImplementation,
  type MaximumWriteLengthFeatureInput,
  type MaximumWriteLengthFeatureOutput
} from '../../backend-contract/capabilities'
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { loadDesktopCoreBinding } from '../../desktop-core-addon'
import {
  createServiceUuidScanPlan,
  diagnosticServiceUuidScanPlan,
  trustedServiceUuidFilter
} from '../scan-planning/service-uuid-scan-planner'
import {
  BLUEZ_BACKEND_ID,
  BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON,
  BLUEZ_PLATFORM_ID,
  COREBLUETOOTH_BACKEND_ID,
  COREBLUETOOTH_PLATFORM_ID,
  WINRT_BACKEND_ID,
  WINRT_PLATFORM_ID,
  bluezCompatibility,
  coreBluetoothCompatibility,
  winRtCompatibility
} from './platform-identity'
import { createBluezConnectionControlRegistrations } from './bluez-connection-capabilities'
import { createBluezPairingGenerationRegistration } from './bluez-pairing-generation-capability'
import { createCoreBluetoothUnsupportedRegistrations } from './desktop-unsupported-capabilities'
import {
  cleanupRecordFromCloseReport,
  decodeDesktopRustCoreCapabilityStates,
  desktopRustCoreError,
  parseDesktopRustCoreWireError,
  desktopRustCoreOperation,
  throwDesktopRustCoreError,
  type DesktopRustCoreAdapterAuthorization,
  type DesktopRustCoreAdapterPower,
  type DesktopRustCoreAdapterResetEvent,
  type DesktopRustCoreAdvertisement,
  type DesktopRustCoreBinding,
  type DesktopRustCoreBluezBus,
  type DesktopRustCoreCapabilityState,
  type DesktopRustCoreGenerationController,
  type DesktopRustCoreCentral,
  type DesktopRustCoreCharacteristicAccess,
  type DesktopRustCoreControl,
  type DesktopRustCoreLifecycleEvent,
  type DesktopRustCoreSourceUncertaintyEvent,
  parseDesktopRustCoreLifecyclePlatform,
  parseDesktopRustCoreReleaseReport,
  type DesktopRustCorePairOutcome,
  type DesktopRustCorePath,
  type DesktopRustCorePlatform,
  type DesktopRustCoreRadio,
  type DesktopRustCoreScanObservation,
  type DesktopRustCoreSecurityEvent,
  type DesktopRustCoreScanTerminalEvent,
  type DesktopRustCoreSecurityState,
  type DesktopRustCoreSelector,
  type DesktopRustCoreConnectionParametersEvent,
  type DesktopRustCoreDurablePeerSourceFailureRecoveryHandle,
  type DesktopRustCorePeerSourceFailureRecoveryChunk,
  type DesktopRustCorePeerSourceFailureRecoveryDescriptor,
  type DesktopRustCoreWireError,
  type DesktopRustCoreWriteReadinessEvent
} from './desktop-rust-core-binding'
import { createDesktopPeerDirectory } from './desktop-peer-directory'

export const DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

/**
 * Explicit, opt-in access to a source failure retained by the desktop core.
 * No method acknowledges implicitly: callers must read the complete raw
 * sequence then call `acknowledge` themselves.
 */
export interface DesktopPeerNotificationFailureRecovery {
  /** Wait for the source-time offer currently blocking this subscription. */
  next(): Promise<DesktopPeerNotificationFailureRecoveryOffer>
  /** Read one exact raw evidence page for the offered recipient. */
  read(offset: string): Promise<DesktopRustCorePeerSourceFailureRecoveryChunk>
  /** Advance this recipient only after its complete contiguous read. */
  acknowledge(): Promise<void>
}

/**
 * One live retained-failure offer. `error` is the native producer's exact
 * structured error envelope, not a diagnostic rendering of the descriptor.
 */
export interface DesktopPeerNotificationFailureRecoveryOffer {
  readonly descriptor: DesktopRustCorePeerSourceFailureRecoveryDescriptor
  readonly error: DesktopRustCoreWireError
}

/**
 * Recovery which outlived the process that observed its source route. The
 * original peer, selector, consumer and lease remain provenance/authority;
 * this does not create a new subscription or claim a fresh route epoch.
 */
export interface DesktopDurablePeerNotificationFailureRecovery {
  readonly provenance: Readonly<{
    readonly peerId: string
    readonly selector: DesktopRustCoreSelector
    readonly consumer: string
    readonly recipientLease: string
    readonly contiguousRead: string
    readonly acknowledged: boolean
    readonly producerAck: 'not-started' | 'in-flight' | 'committed'
  }>
  readonly descriptor: DesktopRustCorePeerSourceFailureRecoveryDescriptor
  /** Reads durable staged bytes. An unresolved native owner never becomes a fresh live route. */
  read(offset: string): Promise<DesktopRustCorePeerSourceFailureRecoveryChunk>
  /**
   * Records this recipient's completion. If the original native owner cannot
   * be reconciled after restart, rejects with the typed restoration outcome
   * while preserving the durable bytes and receipt.
   */
  acknowledge(): Promise<void>
}

/** Frozen per-platform facts of the one desktop Rust provider. */
export interface DesktopRustCoreProfile {
  readonly platform: DesktopRustCorePlatform
  readonly backendId: string
  readonly providerId: string
  readonly platformId: string
  readonly compatibility: BackendCompatibilityOffer
  /** The only `process.platform` this profile runs on. */
  readonly requiredProcessPlatform: 'linux' | 'darwin' | 'win32'
  /** Platform-detail code of the pre-load platform guard. */
  readonly requiredPlatformCode: 'linux-required' | 'macos-required' | 'windows-required'
  /** The host's 4.x operation prefix (`direct-gatt`, `winrt`, `bluez`); see `desktopRustCoreOperation`. */
  readonly operationPrefix: string
  readonly defaultOwner: string
  readonly displayName: string
  /** Why authorization reads `unknown` on this host (never a denial). */
  readonly authorizationReason: string
  /** Observation fields this platform's radio reports (scan planning context). */
  readonly observationFields: readonly ScanObservationField[]
  /**
   * Every desktop profile sets this true. `coreRequirement` does not read
   * it: a hard `require-*` is always forwarded, and a preference is never
   * a requirement. The property check still rejects a mode the
   * characteristic does not support before any dispatch.
   */
  readonly deliveryRequirementToCore: boolean
}

const COMMON_OBSERVATION_FIELDS: readonly ScanObservationField[] = Object.freeze([
  'localName',
  'rssi',
  'serviceUuids',
  'manufacturerData',
  'serviceData'
])
/** BlueZ and WinRT report the peer's radio address; CoreBluetooth masks it. */
const ADDRESSED_OBSERVATION_FIELDS: readonly ScanObservationField[] = Object.freeze([
  ...COMMON_OBSERVATION_FIELDS,
  'address'
])

export const DESKTOP_RUST_CORE_PROFILES: Readonly<Record<DesktopRustCorePlatform, DesktopRustCoreProfile>> =
  Object.freeze({
    bluez: Object.freeze({
      platform: 'bluez',
      backendId: BLUEZ_BACKEND_ID,
      providerId: 'unified-ble:bluez-dbus-provider',
      platformId: BLUEZ_PLATFORM_ID,
      compatibility: bluezCompatibility,
      requiredProcessPlatform: 'linux',
      requiredPlatformCode: 'linux-required',
      operationPrefix: 'bluez',
      defaultOwner: 'node-bluez',
      displayName: 'BlueZ adapter (shared Rust core)',
      authorizationReason: BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON,
      observationFields: ADDRESSED_OBSERVATION_FIELDS,
      deliveryRequirementToCore: true
    }),
    corebluetooth: Object.freeze({
      platform: 'corebluetooth',
      backendId: COREBLUETOOTH_BACKEND_ID,
      providerId: 'unified-ble:corebluetooth-provider',
      platformId: COREBLUETOOTH_PLATFORM_ID,
      compatibility: coreBluetoothCompatibility,
      requiredProcessPlatform: 'darwin',
      requiredPlatformCode: 'macos-required',
      operationPrefix: 'direct-gatt',
      defaultOwner: 'node-corebluetooth',
      displayName: 'CoreBluetooth default adapter (shared Rust core)',
      authorizationReason: 'CoreBluetooth reported no authorization for this process; unmeasured, not denied',
      observationFields: COMMON_OBSERVATION_FIELDS,
      deliveryRequirementToCore: true
    }),
    winrt: Object.freeze({
      platform: 'winrt',
      backendId: WINRT_BACKEND_ID,
      providerId: 'unified-ble:winrt-provider',
      platformId: WINRT_PLATFORM_ID,
      compatibility: winRtCompatibility,
      requiredProcessPlatform: 'win32',
      requiredPlatformCode: 'windows-required',
      operationPrefix: 'winrt',
      defaultOwner: 'node-winrt',
      displayName: 'Windows Bluetooth LE adapter (shared Rust core)',
      authorizationReason: 'Windows reports no Bluetooth authorization for this process',
      observationFields: Object.freeze([...ADDRESSED_OBSERVATION_FIELDS, 'connectable'] as const),
      deliveryRequirementToCore: true
    })
  })

/** `<prefix>.native-boundary.load`: the operation a missing or unloadable core fails with (the 4.x id). */
export function desktopRustCoreMissingOperation(platform: DesktopRustCorePlatform): string {
  return desktopRustCoreOperation(DESKTOP_RUST_CORE_PROFILES[platform].operationPrefix, 'rust-core-missing')
}

/**
 * Refuses a host the profile does not run on, before any addon is loaded
 * (PR210-29): a darwin addon must never drive CoreBluetooth while the
 * caller believes it is talking to BlueZ.
 */
export function assertDesktopRustCorePlatform(
  platform: DesktopRustCorePlatform,
  hostPlatform: string = process.platform
): void {
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  if (hostPlatform !== profile.requiredProcessPlatform) {
    throw contractError(
      'capability.unavailable',
      'platform',
      desktopRustCoreOperation(profile.operationPrefix, 'platform'),
      {
        domain: 'desktop-rust-core',
        code: profile.requiredPlatformCode,
        safeMessage: `The ${platform} backend runs only on ${profile.requiredProcessPlatform}; this process runs on ${hostPlatform}`,
        metadata: Object.freeze({ hostPlatform, requiredPlatform: profile.requiredProcessPlatform })
      }
    )
  }
}

export interface DesktopRustCoreProviderOptions {
  readonly platform: DesktopRustCorePlatform
  /** Owner label for admitted core centrals (host identity). */
  readonly owner: string
  /** Monotonic clock supplied by the host. */
  readonly now: () => number
  /** `node` for Node hosts, `desktop-native` for Electron main. */
  readonly hostKind?: 'node' | 'desktop-native'
  /**
   * Injected core entry (tests and embedding hosts). Absent, the provider
   * loads the packaged addon on first use and verifies its build identity
   * before any radio call; there is no TypeScript fallback.
   */
  readonly binding?: DesktopRustCoreBinding
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /** BlueZ only: the D-Bus bus (the legacy `busKind`); `system` by default. */
  readonly bluezBus?: DesktopRustCoreBluezBus
  readonly connectionPolicy?: BluezConnectionPolicy
  /**
   * BlueZ only: the host's privileged pairing-generation controller. The
   * package never escalates on its own; with a controller, a pair directing
   * `secureConnections` holds the adapter generation for the ceremony and
   * the core restores it afterwards.
   */
  readonly pairingGeneration?: DesktopRustCoreGenerationController
  /**
   * Existing, host-authorized continuation recording which owns durable
   * peer-source failure evidence. This is a trusted Node/Electron-main/Tauri
   * configuration value, never a renderer path or an implicit journal.
   */
  readonly peerFailureRecoveryRecordingId?: string
  /**
   * Trusted host-selected private directory and byte budget for exact native
   * source-failure evidence. This is admitted before radio open and is
   * deliberately unavailable to renderers.
   */
  readonly failureEvidenceStore?: Readonly<{
    readonly directory: string
    /** Decimal positive byte budget so JavaScript never rounds a native integer. */
    readonly maxBytes: string
  }>
  /** Trusted host-selected source-terminal evidence ledger, opened before radio activation. */
  readonly terminalEvidenceStore?: Readonly<{
    readonly directory: string
    readonly maxBytes: string
  }>
}

/**
 * Test-only seams for the shared-core desktop provider (owner decision L).
 * Reachable only through `createTestDesktopRustCoreBackendProvider`, which
 * `unified-ble-manager/testing` re-exports. Production entrypoints never
 * accept these keys: the production factory refuses them fail-closed.
 */
export interface DesktopRustCoreTestProviderOptions extends DesktopRustCoreProviderOptions {
  /**
   * Radio selection. `production` (default) executes the btleplug radio and
   * fails loudly with `adapter.unavailable` where no adapter exists — it
   * never falls back to synthetic. `synthetic` is the hardware-free leg for
   * deterministic suites; the public factories never select it.
   */
  readonly radio?: DesktopRustCoreRadio
  /** Resolves the packaged binding; defaults to `src/desktop-core-addon.ts`. */
  readonly loadBinding?: (profile: DesktopRustCoreProfile) => Promise<DesktopRustCoreBinding>
  /** Test seam for the pre-load platform guard; defaults to `process.platform`. */
  readonly hostPlatform?: string
  /**
   * Test seam for the CoreBluetooth first-usable-state bound; the legacy
   * fixed 10 s (`ADAPTER_INITIALIZATION_TIMEOUT_MS`) otherwise.
   */
  readonly firstStateTimeoutMs?: number
}

/**
 * How long a CoreBluetooth central waits for its first usable adapter state
 * before it is refused (legacy `NATIVE_COREBLUETOOTH_INITIALIZATION_TIMEOUT_MILLISECONDS`):
 * CoreBluetooth reports its state asynchronously after the manager exists,
 * and work issued before it is ignored by the OS.
 */
export const ADAPTER_INITIALIZATION_TIMEOUT_MS = 10_000

/** The legacy platform code of a first-state timeout (the core's detail prefix too). */
const ADAPTER_INITIALIZATION_TIMED_OUT = 'adapter-initialization-timed-out'

interface ListedAdapter {
  readonly adapterId: string
  /** OS adapter label the core selects by; `null` for the synthetic adapter. */
  readonly label: string | null
  /** Windows: the process deployment the OS reported with this adapter. */
  readonly deployment?: 'packaged' | 'unpackaged' | null
  readonly descriptor: AdapterDescriptor<string>
}

/** Production option keys that must never carry a test seam value. */
const DESKTOP_RUST_CORE_TEST_SEAM_KEYS: readonly string[] = Object.freeze([
  'radio',
  'loadBinding',
  'hostPlatform',
  'firstStateTimeoutMs'
])

/**
 * Fail-closed seam guard: a test seam passed to the production factory is
 * refused before anything loads, so a suite that meant to use `/testing`
 * can never silently run the production path (or vice versa).
 */
function assertNoDesktopRustCoreTestSeams(options: DesktopRustCoreProviderOptions): void {
  const profile = DESKTOP_RUST_CORE_PROFILES[options.platform]
  for (const key of DESKTOP_RUST_CORE_TEST_SEAM_KEYS) {
    if (Reflect.get(options, key) !== undefined) {
      throw contractError(
        'argument.invalid',
        'core',
        desktopRustCoreOperation(profile.operationPrefix, 'test-seam-option'),
        {
          domain: 'desktop-rust-core',
          code: 'test-seam-option',
          safeMessage: `option ${key} is test-only: use the unified-ble-manager/testing factory`,
          metadata: Object.freeze({ option: key })
        }
      )
    }
  }
}

/**
 * Creates the shared-core desktop provider for one platform. The platform
 * guard runs here, before anything loads; the addon itself loads lazily on
 * the first `listAdapters`/`create`, where its build identity is checked.
 * Test seams are refused (see `DesktopRustCoreTestProviderOptions`).
 */
export function createDesktopRustCoreBackendProvider(
  options: DesktopRustCoreProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  assertNoDesktopRustCoreTestSeams(options)
  return createDesktopRustCoreBackendProviderWithTestOptions(options)
}

/**
 * Test-only factory for the shared-core desktop provider: the production
 * factory plus the `/testing` seams (synthetic radio, binding loader,
 * platform guard and first-state bound overrides). Re-exported from
 * `unified-ble-manager/testing`; never from a production entrypoint.
 */
export function createTestDesktopRustCoreBackendProvider(
  options: DesktopRustCoreTestProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  return createDesktopRustCoreBackendProviderWithTestOptions(options)
}

function createDesktopRustCoreBackendProviderWithTestOptions(
  options: DesktopRustCoreTestProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const profile = DESKTOP_RUST_CORE_PROFILES[options.platform]
  if (options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner'))
  }
  if (options.peerFailureRecoveryRecordingId !== undefined && !isRecordingId(options.peerFailureRecoveryRecordingId)) {
    throw contractError(
      'argument.invalid',
      'restoration',
      desktopRustCoreOperation(profile.operationPrefix, 'peer-failure-recovery-recording')
    )
  }
  const failureEvidenceStore = options.failureEvidenceStore
  if (failureEvidenceStore !== undefined) {
    if (failureEvidenceStore.directory.length === 0 || !/^[1-9][0-9]*$/.test(failureEvidenceStore.maxBytes)) {
      throw contractError(
        'argument.invalid',
        'restoration',
        desktopRustCoreOperation(profile.operationPrefix, 'failure-evidence-store')
      )
    }
  }
  const terminalEvidenceStore = options.terminalEvidenceStore
  if (terminalEvidenceStore !== undefined) {
    if (terminalEvidenceStore.directory.length === 0 || !/^[1-9][0-9]*$/.test(terminalEvidenceStore.maxBytes)) {
      throw contractError(
        'argument.invalid',
        'restoration',
        desktopRustCoreOperation(profile.operationPrefix, 'terminal-evidence-store')
      )
    }
  }
  assertDesktopRustCorePlatform(options.platform, options.hostPlatform)
  const radio = options.radio ?? 'production'
  const hostKind = options.hostKind ?? 'node'
  const createOwnerId = options.createOwnerId ?? allocateRustCoreOwnerId
  let bindingPromise: Promise<DesktopRustCoreBinding> | null = null
  const binding = (): Promise<DesktopRustCoreBinding> => {
    if (bindingPromise === null) {
      const load =
        options.binding !== undefined
          ? Promise.resolve(options.binding)
          : (options.loadBinding ?? loadPackagedBinding)(profile)
      bindingPromise = load.catch((error: unknown) => {
        bindingPromise = null
        throw error
      })
    }
    return bindingPromise
  }
  const listed = new Map<string, ListedAdapter>()
  if (
    profile.platform !== 'bluez' &&
    (options.bluezBus !== undefined ||
      options.pairingGeneration !== undefined ||
      options.connectionPolicy !== undefined)
  ) {
    throw contractError(
      'argument.invalid',
      'core',
      desktopRustCoreOperation(profile.operationPrefix, 'bluez-only-option')
    )
  }
  const connectionPolicy = admitBluezConnectionPolicy(options.connectionPolicy)
  const context = (resolved: DesktopRustCoreBinding): BackendOpenContext => ({
    profile,
    owner: options.owner,
    now: options.now,
    binding: resolved,
    radio,
    hostKind,
    createOwnerId,
    bluezBus: options.bluezBus ?? 'system',
    connectionPolicy,
    pairingGeneration: options.pairingGeneration ?? null,
    peerFailureRecoveryRecordingId: options.peerFailureRecoveryRecordingId ?? null,
    failureEvidenceStore: options.failureEvidenceStore ?? null,
    terminalEvidenceStore: options.terminalEvidenceStore ?? null,
    firstStateTimeoutMs: options.firstStateTimeoutMs ?? ADAPTER_INITIALIZATION_TIMEOUT_MS
  })
  const list = async (): Promise<readonly ListedAdapter[]> => {
    const resolved = await binding()
    const adapters = await listRustCoreAdapters(context(resolved))
    listed.clear()
    for (const adapter of adapters) listed.set(adapter.adapterId, adapter)
    return adapters
  }
  return Object.freeze({
    descriptor: Object.freeze({
      providerId: profile.providerId,
      hostKind,
      loadability: 'loadable',
      compatibility: profile.compatibility
    }),
    listAdapters: async () => Object.freeze((await list()).map(adapter => adapter.descriptor)),
    create: async (selection: AdapterSelection<string>) => {
      const selected = String(selection.selectedAdapterId)
      const adapter = listed.get(selected) ?? (await list()).find(candidate => candidate.adapterId === selected)
      if (adapter === undefined || adapter.descriptor.state.availability === 'unavailable') {
        throw contractError(
          'adapter.unavailable',
          'adapter',
          desktopRustCoreOperation(profile.operationPrefix, 'select-adapter')
        )
      }
      return openRustCoreBackend(context(await binding()), adapter)
    }
  })
}

function loadPackagedBinding(profile: DesktopRustCoreProfile): Promise<DesktopRustCoreBinding> {
  return loadDesktopCoreBinding(profile)
}

interface BackendOpenContext {
  readonly profile: DesktopRustCoreProfile
  readonly owner: string
  readonly now: () => number
  readonly binding: DesktopRustCoreBinding
  readonly radio: DesktopRustCoreRadio
  readonly hostKind: 'node' | 'desktop-native'
  readonly createOwnerId: () => string
  readonly bluezBus: DesktopRustCoreBluezBus
  readonly connectionPolicy: BluezConnectionPolicy | undefined
  readonly pairingGeneration: DesktopRustCoreGenerationController | null
  readonly peerFailureRecoveryRecordingId: string | null
  readonly failureEvidenceStore: Readonly<{ readonly directory: string; readonly maxBytes: string }> | null
  readonly terminalEvidenceStore: Readonly<{ readonly directory: string; readonly maxBytes: string }> | null
  readonly firstStateTimeoutMs: number
}

let nextRustCoreOwner = 1

function allocateRustCoreOwnerId(): string {
  const ordinal = nextRustCoreOwner
  nextRustCoreOwner += 1
  return `desktop-rust-core-owner-${ordinal}`
}

/** The one CoreBluetooth central adapter's public id (the legacy constant). */
const COREBLUETOOTH_DEFAULT_ADAPTER_ID = 'corebluetooth-default-adapter'

/**
 * The public adapter id for an OS adapter label, in the legacy 4.x form so a
 * persisted `adapterId` keeps selecting the same adapter: CoreBluetooth has
 * one (`corebluetooth-default-adapter`), BlueZ uses the adapter's D-Bus
 * object path (`/org/bluez/hci0`), and WinRT the raw Windows device id. The
 * id depends only on the adapter, never on which adapter is the default or
 * on the listing order, so it is stable while adapters come and go.
 */
export function desktopRustCoreAdapterId(platform: DesktopRustCorePlatform, label: string): string {
  if (platform === 'corebluetooth') return COREBLUETOOTH_DEFAULT_ADAPTER_ID
  if (platform === 'bluez') return `/org/bluez/${label}`
  return label
}

/** An adapter the OS listed without a label: not selectable, but never dropped from the listing. */
function unlabelledAdapterId(index: number): string {
  return `unlabelled-adapter-${index}`
}

/**
 * Enumerate the OS adapters and measure each one's power through a
 * short-lived central (the legacy backends measured state at listing too).
 * An adapter the OS could not describe, or that fails to open, is listed as
 * `unavailable` with the OS's own reason — never dropped from the listing.
 */
async function listRustCoreAdapters(context: BackendOpenContext): Promise<readonly ListedAdapter[]> {
  const { profile } = context
  if (context.radio === 'synthetic') {
    const backend = await openRustCoreBackend(
      context,
      {
        adapterId: desktopRustCoreAdapterId(profile.platform, 'synthetic'),
        label: null,
        descriptor: pendingAdapterDescriptor(profile, desktopRustCoreAdapterId(profile.platform, 'synthetic'), null)
      },
      'listing'
    )
    try {
      return Object.freeze([
        {
          adapterId: String(backend.identity.attachment.adapter.adapterId),
          label: null,
          descriptor: backend.identity.attachment.adapter
        }
      ])
    } finally {
      await cleanupDesktopAllocation(
        contractError('platform.failure', 'cleanup', 'desktop.list-adapters.cleanup'),
        () => backend.destroy()
      )
    }
  }
  let listings
  try {
    listings = await context.binding.listAdapters(context.bluezBus)
  } catch (error) {
    throwDesktopRustCoreError(error, desktopRustCoreOperation(profile.operationPrefix, 'list-adapters'))
  }
  const adapters: ListedAdapter[] = []
  for (const listing of listings) {
    if (typeof listing.label !== 'string' || listing.label.length === 0) {
      const adapterId = unlabelledAdapterId(listing.index)
      adapters.push({
        adapterId,
        label: null,
        descriptor: unavailableAdapterDescriptor(
          profile,
          adapterId,
          `adapter ${listing.index}`,
          listing.error ?? 'the OS returned no adapter label',
          context.now
        )
      })
      continue
    }
    const adapterId = desktopRustCoreAdapterId(profile.platform, listing.label)
    const pending: ListedAdapter = {
      adapterId,
      label: listing.label,
      deployment: listing.deployment ?? null,
      descriptor: pendingAdapterDescriptor(profile, adapterId, listing.displayName ?? null, listing.deployment ?? null)
    }
    let backend: DesktopRustCoreBackend
    try {
      backend = await openRustCoreBackend(context, pending, 'listing')
    } catch (error) {
      if (error instanceof DesktopProcessHostInitializationError) throw error
      const normalized = desktopRustCoreError(
        error,
        desktopRustCoreOperation(profile.operationPrefix, 'list-adapters.open')
      ).normalized
      adapters.push({
        adapterId,
        label: listing.label,
        descriptor: unavailableAdapterDescriptor(
          profile,
          adapterId,
          listing.label,
          `${normalized.code}: ${normalized.platform?.safeMessage ?? normalized.operation}`,
          context.now
        )
      })
      continue
    }
    const descriptor = backend.identity.attachment.adapter
    await cleanupDesktopAllocation(contractError('platform.failure', 'cleanup', 'desktop.list-adapters.cleanup'), () =>
      backend.destroy()
    )
    adapters.push({ adapterId, label: listing.label, deployment: listing.deployment ?? null, descriptor })
  }
  if (adapters.length === 0) {
    throw contractError(
      'adapter.unavailable',
      'adapter',
      desktopRustCoreOperation(profile.operationPrefix, 'list-adapters.none')
    )
  }
  return Object.freeze(adapters)
}

/**
 * Each desktop host's legacy identity formats (origin/main):
 * CoreBluetooth `corebluetooth-backend.ts` / `corebluetooth-attachment-lifecycle.ts` /
 * `corebluetooth-gatt-operations.ts`; WinRT `winrt-backend.ts:450,587-600,994-996,
 * 1164-1166,1474`, `winrt-gatt-operations.ts:698-699`, `winrt-subscription-runtime.ts:313`;
 * BlueZ `bluez-backend.ts:78`, `bluez-backend-runtime.ts:239-251,682-686,1012`,
 * `bluez-connection-runtime.ts:620-677`, `bluez-scan-runtime.ts:38-40,214`,
 * `bluez-subscription-runtime.ts:107`. Generations start at `1` and advance by
 * one per adapter loss; every counter is the backend's own and starts at 1.
 */
interface LegacyDesktopNames {
  readonly attachmentId: (
    instance: string,
    backendGeneration: number,
    adapterId: string,
    adapterGeneration: number
  ) => string
  readonly peer: (backendGeneration: number, ordinal: number) => string
  readonly scanLease: (ordinal: number) => string
  readonly connectionGeneration: (ordinal: number) => string
  /** BlueZ numbered databases per connection record; the others per backend. */
  readonly databasePerConnection: boolean
  readonly databaseGeneration: (ordinal: number) => string
  /** The adapter name the legacy backend published (`null` when the OS gave none). */
  readonly displayName: (osName: string | null) => string | null
}

const LEGACY_DESKTOP_NAMES: Readonly<Record<DesktopRustCorePlatform, LegacyDesktopNames>> = Object.freeze({
  corebluetooth: Object.freeze({
    attachmentId: (instance: string, backendGeneration: number, _adapterId: string, adapterOrdinal: number) =>
      `${instance}:${backendGeneration}:${adapterOrdinal}`,
    peer: (backendGeneration: number, ordinal: number) => `corebluetooth-peer-${backendGeneration}-${ordinal}`,
    scanLease: (ordinal: number) => `corebluetooth-scan-lease-${ordinal}`,
    connectionGeneration: (ordinal: number) => `corebluetooth-connection-generation-${ordinal}`,
    databasePerConnection: false,
    databaseGeneration: (ordinal: number) => `corebluetooth-database-generation-${ordinal}`,
    displayName: () => 'CoreBluetooth default adapter'
  }),
  winrt: Object.freeze({
    attachmentId: (instance: string, backendGeneration: number, _adapterId: string, adapterOrdinal: number) =>
      `${instance}:${backendGeneration}:${adapterOrdinal}`,
    peer: (_backendGeneration: number, ordinal: number) => `winrt-peer-${ordinal}`,
    scanLease: (ordinal: number) => `winrt-scan-lease-${ordinal}`,
    connectionGeneration: (ordinal: number) => String(ordinal),
    databasePerConnection: false,
    databaseGeneration: (ordinal: number) => `winrt-database-generation-${ordinal}`,
    displayName: (osName: string | null) => osName
  }),
  bluez: Object.freeze({
    attachmentId: (instance: string, backendGeneration: number, adapterId: string, adapterOrdinal: number) =>
      `${instance}:${backendGeneration}:${adapterId}:${adapterOrdinal}`,
    peer: (backendGeneration: number, ordinal: number) => `bluez-peer-${backendGeneration}-${ordinal}`,
    scanLease: (ordinal: number) => `bluez-scan-${ordinal}`,
    connectionGeneration: (ordinal: number) => String(ordinal),
    databasePerConnection: true,
    databaseGeneration: (ordinal: number) => String(ordinal),
    displayName: (osName: string | null) => osName
  })
})

/** Legacy numbered backend instances per host module and process, from 1. */
const nextLegacyInstance: Record<DesktopRustCorePlatform, number> = { corebluetooth: 1, winrt: 1, bluez: 1 }

function allocateLegacyInstance(platform: DesktopRustCorePlatform): string {
  const ordinal = nextLegacyInstance[platform]
  nextLegacyInstance[platform] += 1
  return `${platform}-backend-${ordinal}`
}

function adapterGeneration(profile: DesktopRustCoreProfile) {
  // Legacy listings reported the first adapter generation, `1`.
  return opaqueId('1', 'adapter-generation', `${profile.platform}-rust-core`)
}

function pendingAdapterDescriptor(
  profile: DesktopRustCoreProfile,
  adapterId: string,
  osName: string | null,
  deployment: 'packaged' | 'unpackaged' | null = null
): AdapterDescriptor<string> {
  return Object.freeze({
    adapterId: opaqueId(adapterId, 'adapter', `${profile.platform}-rust-core`),
    displayName: LEGACY_DESKTOP_NAMES[profile.platform].displayName(osName),
    state: Object.freeze({
      availability: 'unknown' as const,
      authorization: 'unknown' as const,
      power: 'unknown' as const,
      backendGeneration: opaqueId('1', 'backend-generation', `${profile.platform}-rust-core`),
      updatedAt: monotonicTimestamp(0),
      safeReason: 'adapter state loads when a central opens on it'
    }),
    adapterGeneration: adapterGeneration(profile),
    limitations: Object.freeze([
      'The shared Rust core owns radio scheduling; this backend carries no TypeScript radio policy',
      // The legacy WinRT adapter record's deployment limitation.
      ...(deployment === null ? [] : [`Selected through ${deployment} Windows application deployment semantics`])
    ])
  })
}

function unavailableAdapterDescriptor(
  profile: DesktopRustCoreProfile,
  adapterId: string,
  label: string,
  reason: string,
  now: () => number
): AdapterDescriptor<string> {
  const pending = pendingAdapterDescriptor(profile, adapterId, label)
  return Object.freeze({
    ...pending,
    state: Object.freeze({
      ...pending.state,
      availability: 'unavailable' as const,
      updatedAt: monotonicTimestamp(now()),
      safeReason: reason.slice(0, 512)
    })
  })
}

async function openRustCoreBackend(
  context: BackendOpenContext,
  adapter: ListedAdapter,
  purpose: 'listing' | 'backend' = 'backend'
): Promise<DesktopRustCoreBackend> {
  const { profile } = context
  const sessionOwner = context.createOwnerId()
  if (sessionOwner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner-id'))
  }
  const coreOwner = `${context.owner}/${sessionOwner}`
  // Open-then-admit without leaking: a central that fails to open never
  // becomes a backend, and a backend that fails to open is destroyed before
  // the rejection propagates.
  let central: DesktopRustCoreCentral
  try {
    central =
      context.radio === 'synthetic'
        ? await context.binding.openSynthetic(coreOwner, {
            platform: profile.platform,
            ...(context.pairingGeneration === null ? {} : { pairingGeneration: true }),
            ...(context.terminalEvidenceStore === null
              ? {}
              : {
                  terminalEvidenceDirectory: context.terminalEvidenceStore.directory,
                  terminalEvidenceMaxBytes: context.terminalEvidenceStore.maxBytes
                })
          })
        : await context.binding.openProduction({
            owner: coreOwner,
            platform: profile.platform,
            adapterId: adapter.label,
            ...(profile.platform === 'bluez' ? { bluezBus: context.bluezBus } : {}),
            ...(context.connectionPolicy === undefined ? {} : { connectionPolicy: context.connectionPolicy }),
            ...(context.pairingGeneration === null ? {} : { pairingGeneration: true }),
            ...(context.failureEvidenceStore === null
              ? {}
              : {
                  failureEvidenceDirectory: context.failureEvidenceStore.directory,
                  failureEvidenceMaxBytes: context.failureEvidenceStore.maxBytes
                }),
            ...(context.terminalEvidenceStore === null
              ? {}
              : {
                  terminalEvidenceDirectory: context.terminalEvidenceStore.directory,
                  terminalEvidenceMaxBytes: context.terminalEvidenceStore.maxBytes
                })
          })
  } catch (error) {
    throwDesktopRustCoreError(error, desktopRustCoreOperation(profile.operationPrefix, 'rust-core-open'))
  }
  let backend: DesktopRustCoreBackend | undefined
  try {
    backend = new DesktopRustCoreBackend({
      profile,
      owner: coreOwner,
      terminalEvidenceRecipient: context.owner,
      central,
      now: context.now,
      radio: context.radio,
      hostKind: context.hostKind,
      adapter: adapter.descriptor,
      bindingDiagnostics: Object.freeze({
        ...(context.binding.diagnostics ?? {}),
        ...(adapter.deployment === undefined || adapter.deployment === null ? {} : { deployment: adapter.deployment })
      }),
      coreStates: decodeDesktopRustCoreCapabilityStates(
        await central.runtimeCapabilityStates(),
        desktopRustCoreOperation(profile.operationPrefix, 'capability-states.instance')
      ),
      pairingGeneration: context.pairingGeneration,
      terminalEvidenceConfigured: context.terminalEvidenceStore !== null,
      firstStateTimeoutMs: context.firstStateTimeoutMs,
      // Legacy listings opened no backend: a listing probe takes no instance number.
      backendInstance:
        purpose === 'listing' ? `${profile.platform}-backend-listing` : allocateLegacyInstance(profile.platform)
    })
    if (context.peerFailureRecoveryRecordingId !== null) {
      const attach = central.attachPeerFailureRecoveryRecording
      if (attach === undefined) {
        throw contractError(
          'capability.unsupported',
          'restoration',
          desktopRustCoreOperation(profile.operationPrefix, 'peer-failure-ledger')
        )
      }
      await attach.call(central, context.peerFailureRecoveryRecordingId)
    }
    await backend.open()
    return backend
  } catch (error) {
    const allocatedBackend = backend
    return compensateDesktopInitialization(error, () =>
      allocatedBackend === undefined
        ? central
            .close()
            .then(report =>
              cleanupRecordFromCloseReport(report, desktopRustCoreOperation(profile.operationPrefix, 'session.dispose'))
            )
        : allocatedBackend.destroy()
    )
  }
}

interface DesktopRustCoreBackendConstruction {
  readonly profile: DesktopRustCoreProfile
  readonly owner: string
  /** Stable trusted host identity for direct-Node terminal recovery across backend sessions. */
  readonly terminalEvidenceRecipient: string
  readonly central: DesktopRustCoreCentral
  readonly now: () => number
  readonly radio: DesktopRustCoreRadio
  readonly hostKind: 'node' | 'desktop-native'
  readonly adapter: AdapterDescriptor<string>
  readonly bindingDiagnostics: Readonly<Record<string, string>>
  /** The core's capability registration for this profile's OS (from the loaded binary). */
  readonly coreStates: readonly DesktopRustCoreCapabilityState[]
  readonly pairingGeneration: DesktopRustCoreGenerationController | null
  /** The trusted host explicitly configured the native terminal-evidence ledger. */
  readonly terminalEvidenceConfigured: boolean
  readonly firstStateTimeoutMs: number
  /** The legacy backend instance (`{host}-backend-{n}`); a listing probe takes none. */
  readonly backendInstance: string
}

interface CoreDescriptorNode {
  readonly uuid: string
  readonly occurrence: number
}

interface CoreCharacteristicNode {
  readonly uuid: string
  readonly occurrence: number
  readonly properties: number
  readonly access: DesktopRustCoreCharacteristicAccess | null
  readonly descriptors: readonly CoreDescriptorNode[]
}

interface CoreServiceNode {
  readonly primary: boolean | null
  readonly includedServices: readonly CoreDescriptorNode[] | null
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: readonly CoreCharacteristicNode[]
  readonly restriction?: ReturnType<typeof serviceAccessRestriction>
}

interface StoredCoreCharacteristic {
  readonly characteristic: CoreCharacteristicNode
  readonly characteristicOccurrence: string
  readonly descriptors: readonly string[]
}

interface StoredCoreService {
  readonly service: CoreServiceNode
  readonly serviceOccurrence: string
  readonly characteristics: readonly StoredCoreCharacteristic[]
}

interface StoredCoreDatabase {
  readonly base: DatabasePath<string, string, string>
  readonly services: readonly StoredCoreService[]
  readonly connectionId: string
  readonly nativeConnectionGeneration: string | null
  readonly nativeDatabaseGeneration: string | null
}

interface ConnectionRecord {
  readonly nativePeerId: string
  readonly lease: string
  readonly coreGeneration: string
  readonly path: ConnectionPath<string, string>
  state: 'connected' | 'disconnecting' | 'disconnected' | 'lost'
  parameterSourceFailure?: NormalizedBleError | null
  parameterSourceRevision?: number
  releasePlatform?: PlatformErrorDetail
  requestedTerminalAnnounced?: boolean
  requestedDisconnect?: boolean
  releaseAttempt?: Promise<BackendConnectionCleanupRecord>
  readonly databases: Set<string>
  readonly subscriptions: Set<string>
  readonly acquiredClosers: Set<() => Promise<CleanupRecord>>
  /** The next database ordinal of this link (BlueZ numbered databases per connection record). */
  nextDatabase: number
}

interface SubscriptionRecord {
  readonly nativePeerId: string
  readonly selector: DesktopRustCoreSelector
  readonly consumer: string
  readonly connectionId: string
  readonly notifications: CoreBoundedStream<NotificationValue>
  readonly delivery: 'notification' | 'indication' | 'unknown'
  /** The consumer's overflow policy, applied in the core too (finding 131). */
  readonly overflowPolicy: OverflowPolicy
  readonly terminalEvidenceScope: {
    readonly managerId: string
    readonly operationId: string
    readonly incarnation: string
  } | null
  closed: boolean
  /** Items the core dropped or lost upstream that this stream already reported. */
  surfacedLoss: number
  /** The wake sequence at the last read of the core counters. */
  countersCheckedAt: number
  emptyPolls: number
  /** A V2 retained source failure. The pump pauses until explicit ACK. */
  recovery: DesktopPeerNotificationFailureRecoveryOffer | null
  readonly recoveryWaiters: Set<() => void>
  /**
   * The terminal a lifecycle event decided (link lost, database changed,
   * adapter lost). The stream ends with it only after the values the core
   * still holds for this consumer are delivered (LEGACY-AUDIT-4 R2).
   */
  pendingTerminal: SubscriptionLifecycleTerminal | null
}

/** A notification stream end a lifecycle event decides. */
type SubscriptionLifecycleTerminal = 'connection-lost' | 'service-changed' | 'source-failed'

interface ParameterWatch {
  readonly record: ConnectionRecord
  readonly stream: CoreBoundedStream<ConnectionParametersStreamObservation<string>>
  ordinal: number
  acceptanceRevision: number
  probed: boolean
  failure: NormalizedBleError | null
  readonly openingCancellation: AbortController
  readonly buffered: {
    readonly intervalUs: number
    readonly latency: number
    readonly supervisionTimeoutUs: number
    readonly observedAtMonotonicMs: number
  }[]
}

interface ReadinessWatch {
  readonly record: ConnectionRecord
  readonly stream: CoreBoundedStream<ConnectionWriteReadinessObservation<string>>
  ordinal: number
  acceptanceRevision: number
  failure: NormalizedBleError | null
  readonly openingCancellation: AbortController
  /** The last reported state (F2: the reprobe runs while this is false). */
  ready: boolean
  /** The initial probe resolved (F3: earlier reports buffer, never drop). */
  probed: boolean
  /** The latest report that arrived before the probe resolved (F3). */
  buffered: { readonly ready: boolean; readonly generation: string | null } | null
  /** The pending 100 ms reprobe timer, if any (F2). */
  reprobeTimer: ReturnType<typeof setTimeout> | null
  /** The caller's deadline bounding the reprobe loop, if any (F2). */
  readonly deadline: number | null
  /** The caller's signal bounding the reprobe loop, if any (F2). */
  readonly signal: AbortSignal | null | undefined
}

interface ScanConsumer {
  readonly leaseId: LeaseId<string, string>
  readonly options: OwnerScanOptions<string, string>
  readonly filter: ScanFilter
  readonly stream: CoreBoundedStream<AdvertisementObservation<string>>
  readonly seenPeers: Set<string>
  /** Observations the core handed over malformed while this consumer was live (cumulative). */
  malformedDropped: number
  removeAbort: () => void
}

interface ScanGroup {
  readonly coreOperationId: string
  readonly scanSessionId: ScanSessionId<string, string>
  readonly ownerLeaseId: LeaseId<string, string>
  readonly shareToken: ScanShareToken<string, string> | null
  readonly consumers: Map<string, ScanConsumer>
  state: 'active' | 'stopping' | 'stopped'
  stopResult: Promise<CleanupRecord> | null
  /** Observations the core queued for another scan, refused (never attributed here). */
  foreignRefused: number
  /** Advertising packet and scan response of one peer, for this scan only. */
  readonly evidence: ScanEvidenceSession
}

const NOTIFICATION_BYTES = 512
const ADVERTISEMENT_BYTES = 512
const EVENT_PUMP_INTERVAL_MS = 10
/** Idle notification polls between reads of the core's consumer counters (~100 ms). */
const CORE_COUNTER_POLLS = 20
/** Backstop poll interval of the scan and notification pumps. */
const PUMP_INTERVAL_MS = 5
/** Characteristic property bits: READ, WRITE, WRITE_NO_RESPONSE, NOTIFY, INDICATE. */
const PROPERTY_NOTIFY = 0x08
const PROPERTY_INDICATE = 0x10

/**
 * Shared-core backend: every BLE data-path method dispatches through the
 * admitted dispatch central. The core owns admission, deadlines, overflow,
 * CCCD sharing and teardown. TypeScript retains identity mapping, the scan
 * plan and fan-out, the delivery property check, and event mapping.
 */
export class DesktopRustCoreBackend implements BleCentralBackend<string, HostNeutralBackendIdentity<string>> {
  private processOwnedRecording = false
  private readonly orderedRecorderSessions = new Map<
    string,
    {
      databaseId: string
      readonly startHandle: string
      readonly peerId: string
      active: boolean
      finished: boolean
      receiptRetired: boolean
      recovery: string | null
      handoff: OrderedRecordingRecoveryHandle | null
    }
  >()
  private readonly orderedRecoveryHandles = new WeakMap<
    OrderedRecordingRecoveryHandle,
    { readonly recordingId: string; readonly startHandle: string; busy: boolean }
  >()
  /** Called by the trusted process host before it publishes any child manager. */
  enableProcessOwnedRecording(): void {
    this.processOwnedRecording = true
  }
  private readonly orderedFinishedObservers = new Set<
    (id: string, result: import('../../core/continuation-recording').OrderedRecordingHostStopResult) => Promise<void>
  >()
  /** Trusted process host can reclaim session receipts only after authoritative cleanup. */
  observeOrderedRecordingFinished(
    observer: (
      id: string,
      result: import('../../core/continuation-recording').OrderedRecordingHostStopResult
    ) => Promise<void>
  ): () => void {
    this.orderedFinishedObservers.add(observer)
    return () => {
      this.orderedFinishedObservers.delete(observer)
    }
  }
  /** Trusted process-host seam. Shares this backend's native owner, never opens another central. */
  nativeContinuationController() {
    this.assertAlive('desktop-process-host.continuation')
    return createNativeContinuationController(this.central, async (id, result) => {
      for (const observer of this.orderedFinishedObservers) await observer(id, result)
      this.central.forgetOrderedRecordingSession?.call(this.central, id)
      const session = this.orderedRecorderSessions.get(id)
      if (session) {
        session.active = false
        session.finished = true
        session.recovery = null
        if (session.receiptRetired) this.orderedRecorderSessions.delete(id)
      }
    })
  }
  /** Trusted bridge operations; claim preparation never acknowledges its payload. */
  nativeContinuationControlAccess() {
    this.assertAlive('desktop-process-host.continuation')
    return createNativeContinuationControlAccess(this.central)
  }
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly peers: PeerDirectoryBackend<string> | undefined
  /**
   * Link security through the core's OS adapters (WinRT
   * DeviceInformationPairing, BlueZ Device1.Pair + Agent1), present only
   * where the core implements it on this OS.
   */
  readonly security: SecurityBackend | undefined
  readonly features: FeatureRegistry

  private readonly profile: DesktopRustCoreProfile
  private readonly owner: string
  private readonly terminalEvidenceRecipient: string
  private readonly central: DesktopRustCoreCentral
  private readonly now: () => number
  private readonly radio: DesktopRustCoreRadio
  private readonly hostKind: 'node' | 'desktop-native'
  private readonly bindingDiagnostics: Readonly<Record<string, string>>
  private readonly firstStateTimeoutMs: number
  private identifiers: AttachmentBoundIdFactory<string>
  private attachment: AttachmentRecord<string>
  /** Advances once per core adapter reset (LEGACY-AUDIT-1 #57); handles of an older generation are stale. */
  private generation = 1
  /** The legacy backend instance name (`{host}-backend-{n}`). */
  private readonly backendInstance: string
  // Legacy per-backend resource counters, each from 1.
  private nextPeer = 1
  private nextScan = 1
  private nextConnection = 1
  private nextDatabase = 1
  private nextSubscription = 1
  private adapterState: AdapterStateSnapshot<string>
  private readonly peerIdsByNativeId = new Map<string, PeerId<string>>()
  private readonly nativeIdsByPeerId = new Map<string, string>()
  /**
   * Acquisitions in flight per native peer (finding 194). The caller above
   * the provider can settle its own deadline without cancelling the backend
   * call, leaving the core's `Connecting` claim live; a newer connect for
   * the same peer then supersedes it (cancel plus settle) so arbitration
   * admits the retry instead of refusing `connection.already-owned`.
   * BlueZ joins shared links and never consults this map.
   */
  private readonly pendingAcquisitions = new Map<string, { ticket: string; settled: Promise<void> }>()
  private readonly connectionsById = new Map<string, ConnectionRecord>()
  private readonly subscriptions = new Map<string, SubscriptionRecord>()
  private readonly databases = new Map<string, StoredCoreDatabase>()
  private readonly occurrenceNumerals = new Map<string, number>()
  private readonly ticketsByCorrelation = new Map<string, string>()
  private readonly terminalReservationIds = new Map<string, string>()
  /** Trusted recovery-owner claims captured at staging/recovery, never IPC input. */
  private readonly terminalEvidenceAcknowledgements = new Map<
    string,
    {
      readonly producer: 'scan' | 'subscription'
      readonly recipient: string
      readonly reservationId?: string
      readonly leaseId?: string
    }
  >()
  private readonly adapterTransitions = new Set<CoreBoundedStream<AdapterStateSnapshot<string>>>()
  private readonly securityWatches = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly securityObservedWatches = new WeakSet<CoreBoundedStream<PeerSecurityEvent>>()
  private readonly pendingAddresses = new Map<string, PeerAddressDescriptor>()
  private readonly addressTypes = new Map<string, 'public' | 'random' | null>()
  private readonly readinessWatches = new Set<ReadinessWatch>()
  private readonly parameterWatches = new Set<ParameterWatch>()
  /**
   * Connections with one GATT verb in flight (F6, CoreBluetooth only): the
   * legacy dispatcher admitted one verb per connection and failed a second
   * concurrent verb fast with `lifecycle.invalid-state`.
   */
  private readonly gattVerbsInFlight = new Set<string>()
  private readonly wiring: DesktopRustCoreWiring
  /** One stream per `events()` caller (each manager on this backend), as the 4.x backends fanned out. */
  private readonly eventStreams = new Set<OwnedCoreBoundedStream<BackendEvent<string>>>()
  private scanGroup: ScanGroup | null = null
  /** The provider is the sole reader of the core-wide scan ingress queue. */
  private scanPumpRunning: Promise<void> | null = null
  private nextOrdinalValue = 1
  private nextEventOrdinalValue = 1
  /** Highest adapter-state event sequence applied from the core. */
  private adapterStateSequence = 0
  /** The last link-security sequence delivered (a lag re-read continues after it). */
  private securitySequence = 0
  /** Operations dispatched to the core and not yet settled (a live count, never a total). */
  private dispatchedOperations = 0
  private destroyed = false
  /** The core closed an event stream while this backend was live: its facts can no longer arrive. */
  private coreEventsClosed = false
  private opened = false
  private attached = false
  private eventPump: ReturnType<typeof setTimeout> | null = null
  /** Scan and notification pumps waiting for their next poll; a wake polls them now. */
  private readonly pumpWaiters = new Set<() => void>()
  /** Advances on every wake, so a pump can tell a wake raced its poll. */
  private wakeSequence = 0
  private eventPumpRunning: Promise<void> | null = null
  /** A wake arrived while a drain turn ran: run another when it ends. */
  private drainRequested = false
  private destroyResult: Promise<CleanupRecord> | null = null
  private readonly terminalEvidenceConfigured: boolean

  constructor(construction: DesktopRustCoreBackendConstruction) {
    const { profile, owner } = construction
    this.profile = profile
    this.owner = owner
    this.terminalEvidenceRecipient = construction.terminalEvidenceRecipient
    this.central = construction.central
    this.now = construction.now
    this.radio = construction.radio
    this.hostKind = construction.hostKind
    this.bindingDiagnostics = construction.bindingDiagnostics
    this.terminalEvidenceConfigured = construction.terminalEvidenceConfigured
    this.firstStateTimeoutMs = construction.firstStateTimeoutMs
    this.backendInstance = construction.backendInstance
    this.adapterState = construction.adapter.state
    this.attachment = this.attachmentFor(construction.adapter, construction.adapter.state)
    this.adapterState = this.attachment.adapter.state
    this.identifiers = this.identifiersFor(this.attachment)
    this.wiring = desktopRustCoreWiring(construction.coreStates)
    this.features = createDesktopRustCoreFeatureRegistry(
      profile,
      this.wiring,
      Object.freeze({ invoke: (input: MaximumWriteLengthFeatureInput) => this.observeMaximumWriteLength(input) })
    )
    this.security = this.wiring.security ? this.createSecurityBackend() : undefined
    this.peers = this.wiring.peerDirectory
      ? createDesktopPeerDirectory({
          backendId: this.profile.backendId,
          generation: () => this.generation,
          connected: (services, options) =>
            this.runPeerQuery('peers.connected', options, control =>
              this.central.connectedPeers({ services, ...control })
            ),
          resolve: (peerId, options) =>
            this.runPeerQuery('peers.resolve', options, control => this.central.resolvePeer({ peerId, ...control })),
          bonded: options => this.runPeerQuery('peers.bonded', options, control => this.central.bondedPeers(control)),
          ...(this.profile.platform === 'corebluetooth'
            ? {}
            : {
                known: (options: BackendPeerQuery) =>
                  this.runPeerQuery('peers.known', options, control => this.central.knownDirectoryPeers(control))
              }),
          peerId: nativeId => this.peerIdForNativeId(nativeId),
          assertUsable: (operation, options) => this.assertPeerQueryUsable(operation, options)
        })
      : undefined
    const controller = construction.pairingGeneration
    if (controller !== null) {
      this.central.installPairingGenerationController(
        adapterId => controller.read(adapterId),
        (adapterId, generation) => controller.set(adapterId, pairingGenerationValue(generation))
      )
    }
    this.adapter = Object.freeze({
      currentState: () => this.currentAdapterState(),
      watchState: async () => this.watchAdapterState()
    })
    this.scanner = Object.freeze({
      plan: (query: NormalizedScanQuery) => diagnosticServiceUuidScanPlan(this.planScan(query)),
      ...(this.hasTerminalEvidenceSurface()
        ? {
            reserveTerminalEvidence: (request: TerminalEvidenceReservationRequest) =>
              this.reserveTerminalEvidence(request),
            readTerminalEvidence: (reference: string, maximumBytes: number) =>
              this.readTerminalEvidence(reference, maximumBytes),
            releaseTerminalEvidence: (reference: string) => this.releaseTerminalEvidence(reference)
          }
        : {}),
      start: (options: OwnerScanOptions<string, string>, clientId: ClientId<string, string>) =>
        this.startScan(options, clientId),
      join: (
        sharedLeaseId: LeaseId<string, string>,
        shareToken: ScanShareToken<string, string>,
        clientId: ClientId<string, string>
      ) => this.joinScan(sharedLeaseId, shareToken, clientId)
    })
    const connections: ConnectionBackend<string> = {
      connect: (peerId: PeerId<string>, clientId: ClientId<string, string>, options: ConnectionOptions) =>
        this.connect(peerId, clientId, options)
    }
    this.connections = Object.freeze({
      ...connections,
      ...(this.wiring.rssi
        ? {
            readRssi: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: ReadRssiRequest<string, Operation>
            ) => this.readRssi(connection, request)
          }
        : {}),
      ...(this.wiring.effectiveMtu
        ? {
            effectiveMtu: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: EffectiveMtuRequest<string, Operation>
            ) => this.effectiveMtu(connection, request)
          }
        : {}),
      ...(this.wiring.phy
        ? {
            readPhy: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: ReadPhyRequest<string, Operation>
            ) => this.readPhy(connection, request)
          }
        : {}),
      ...(this.wiring.priority
        ? {
            requestPriority: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: RequestPriorityRequest<string, Operation>
            ) => this.requestPriority(connection, request)
          }
        : {}),
      ...(this.wiring.maximumWriteLength
        ? {
            maximumWriteLength: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: ConnectionMaximumWriteLengthRequest<string, Operation>
            ) => this.maximumWriteLength(connection, request)
          }
        : {}),
      ...(this.wiring.addressTargeting
        ? { peerFromAddress: (descriptor: PeerAddressDescriptor) => this.peerFromAddress(descriptor) }
        : {}),
      ...(this.wiring.writeReadiness
        ? {
            writeWithoutResponseReadiness: (
              connection: BackendConnection<string, string>,
              options?: PublicOperationOptions
            ) => this.writeWithoutResponseReadiness(connection, options)
          }
        : {}),
      ...(this.wiring.connectionParameters
        ? {
            parameters: <Operation extends string>(
              connection: BackendConnection<string, string>,
              request: ConnectionParametersRequest<string, Operation>
            ) => this.readConnectionParameters(connection, request),
            parameterEvents: (connection: BackendConnection<string, string>, options?: PublicOperationOptions) =>
              this.watchConnectionParameters(connection, options)
          }
        : {})
    })
    this.gatt = Object.freeze({
      ...(this.hasTerminalEvidenceSurface()
        ? {
            reserveTerminalEvidence: (request: TerminalEvidenceReservationRequest) =>
              this.reserveTerminalEvidence(request),
            readTerminalEvidence: (reference: string, maximumBytes: number) =>
              this.readTerminalEvidence(reference, maximumBytes),
            releaseTerminalEvidence: (reference: string) => this.releaseTerminalEvidence(reference)
          }
        : {}),
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
      writeWhenReady: (
        path: CharacteristicPath<string, string, string, string, string, 'current'>,
        request: WriteRequest<string, string>
      ) => this.write(path, request, 'gatt.write-when-ready', true),
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
  }

  /**
   * The attachment of the current generation. It is frozen per generation:
   * identity equality covers the adapter state (including updatedAt), so
   * live radio state never rewrites it (live state is
   * `adapter.currentState()`); only an adapter loss advances it.
   */
  private attachmentFor(
    adapter: AdapterDescriptor<string>,
    state: AdapterStateSnapshot<string>
  ): AttachmentRecord<string> {
    // The legacy attachment of this host: backend and adapter generations
    // advance together from 1 (legacy `advanceGeneration`).
    const scope = `${this.profile.platform}-rust-core`
    const backendGeneration = opaqueId(String(this.generation), 'backend-generation', scope)
    return Object.freeze({
      attachmentId: opaqueId(
        LEGACY_DESKTOP_NAMES[this.profile.platform].attachmentId(
          this.backendInstance,
          this.generation,
          String(adapter.adapterId),
          this.generation
        ),
        'attachment',
        scope
      ),
      backendInstanceId: opaqueId(this.backendInstance, 'backend-instance', scope),
      backendGeneration,
      adapter: Object.freeze({
        ...adapter,
        adapterGeneration: opaqueId(String(this.generation), 'adapter-generation', scope),
        state: Object.freeze({ ...state, backendGeneration, updatedAt: monotonicTimestamp(this.now()) })
      })
    })
  }

  private identifiersFor(attachment: AttachmentRecord<string>): AttachmentBoundIdFactory<string> {
    return createAttachmentBoundIdFactory<string>({
      attachmentId: attachment.attachmentId,
      backendInstanceId: attachment.backendInstanceId,
      backendGeneration: attachment.backendGeneration,
      adapterId: attachment.adapter.adapterId,
      adapterGeneration: attachment.adapter.adapterGeneration
    })
  }

  /** Reads the adapter's power from the core, freezes the attachment, and starts the event pump. */
  async open(): Promise<void> {
    if (this.profile.platform === 'corebluetooth') await this.awaitFirstUsableState()
    this.adapterState = await this.measureAdapterState(
      desktopRustCoreOperation(this.profile.operationPrefix, 'open.adapter-state')
    )
    this.attachment = Object.freeze({
      ...this.attachment,
      adapter: Object.freeze({ ...this.attachment.adapter, state: this.adapterState })
    })
    this.opened = true
    try {
      this.central.setEventWaker(() => this.onCoreWake())
    } catch (error) {
      throwDesktopRustCoreError(error, desktopRustCoreOperation(this.profile.operationPrefix, 'open.event-waker'))
    }
    this.scheduleEventPump()
  }

  /**
   * CoreBluetooth reports its state asynchronously after the central exists:
   * wait for the first usable one, as the legacy backend did, and refuse
   * with its error (`capability.unavailable`, platform code
   * `adapter-initialization-timed-out`) when none arrives in time.
   */
  private async awaitFirstUsableState(): Promise<void> {
    const operation = desktopRustCoreOperation(this.profile.operationPrefix, 'adapter.initialize')
    try {
      await this.central.awaitUsableAdapter(this.firstStateTimeoutMs)
    } catch (error) {
      const normalized = desktopRustCoreError(error, operation)
      const detail = normalized.normalized.platform?.safeMessage ?? ''
      if (
        normalized.normalized.code === 'capability.unavailable' &&
        detail.startsWith(ADAPTER_INITIALIZATION_TIMED_OUT)
      ) {
        throw contractError('capability.unavailable', 'platform', operation, {
          domain: 'corebluetooth',
          code: ADAPTER_INITIALIZATION_TIMED_OUT,
          safeMessage: 'CoreBluetooth did not report a usable adapter state before the initialization deadline',
          metadata: Object.freeze({ core: detail.slice(0, 512) })
        })
      }
      throw normalized
    }
  }

  get identity(): HostNeutralBackendIdentity<string> {
    return Object.freeze({
      registeredBackendId: this.profile.backendId,
      registeredPlatformId: this.profile.platformId,
      attachment: this.attachment,
      versions: negotiateCoreVersions(this.profile.compatibility, this.profile.compatibility),
      runtime: Object.freeze({
        hostKind: this.hostKind,
        implementationVersion: DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          ...this.bindingDiagnostics,
          boundary: 'desktop-rust-core-v2',
          transport: 'napi-UbmCentral',
          platform: this.profile.platform,
          radio: this.radio
        })
      })
    })
  }

  /**
   * Admits one attachment: the caller's core offer is negotiated against
   * this profile's (a skewed offer is `protocol.incompatible`, a malformed
   * one `protocol.malformed`), and a second attach is refused, as the 4.x
   * desktop backends refused both.
   */
  async attach(
    request: BackendAttachmentRequest
  ): Promise<BackendAttachment<string, HostNeutralBackendIdentity<string>>> {
    const operation = this.op('attach')
    this.assertAlive(operation)
    if (!this.opened) {
      throw contractError('lifecycle.invalid-state', 'core', this.op('attach-before-open'))
    }
    if (this.attached) throw contractError('lifecycle.invalid-state', 'core', operation)
    negotiateCoreVersions(this.profile.compatibility, request.coreCompatibility)
    this.attached = true
    return Object.freeze({ attachment: this.attachment, identity: this.identity })
  }

  /**
   * A new event stream for this caller: every manager attached to the
   * backend (an owner and its borrowers) sees every event, and closing one
   * stream never ends another's.
   */
  events(): BoundedAsyncStream<BackendEvent<string>> {
    this.assertAlive(this.op('events'))
    const stream: OwnedCoreBoundedStream<BackendEvent<string>> = new OwnedCoreBoundedStream<BackendEvent<string>>(
      // F10: legacy backend-event quotas (64/64KiB/1).
      { itemCapacity: capacity(64), byteCapacity: capacity(64 * 1024), reservedControlCapacity: capacity(1) },
      'drop-oldest',
      () => this.eventStreams.delete(stream)
    )
    if (this.coreEventsClosed) stream.closeWithReason('source-failed', this.coreEventsClosedError())
    else this.eventStreams.add(stream)
    return stream
  }

  /** Rust-side execution witness: per-verb counts the core admitted for this central. */
  dispatchCounters(): Readonly<Record<string, number>> {
    return this.central.dispatchCounters()
  }

  resourceCounters(): ResourceCounters {
    if (!this.opened) {
      throw contractError('lifecycle.invariant-violation', 'core', this.op('counters-unavailable'))
    }
    const scanConsumers = this.scanGroup?.consumers.size ?? 0
    const liveConnections = [...this.connectionsById.values()].filter(
      record => record.state === 'connected' || record.state === 'disconnecting'
    )
    // A physical link is per peer: several leases may share one peer's link,
    // so the counter counts distinct peers, never leases.
    const linkedPeers = new Set(liveConnections.map(record => record.nativePeerId))
    return Object.freeze({
      activeScanControllers: resourceCount(this.scanGroup === null ? 0 : 1),
      scanConsumers: resourceCount(scanConsumers),
      chooserSessions: resourceCount(0),
      connectionLeases: resourceCount(liveConnections.length),
      physicalLinks: resourceCount(linkedPeers.size),
      databaseSnapshots: resourceCount(this.databases.size),
      physicalCccdEnablements: resourceCount(this.subscriptions.size),
      subscriptionConsumers: resourceCount(this.subscriptions.size),
      queuedOperations: resourceCount(0),
      dispatchedOperations: resourceCount(this.dispatchedOperations),
      retainedByteBuffers: resourceCount(0),
      restorationRecords: resourceCount(0),
      orphanedIpcOwners: resourceCount(0)
    })
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

  private async destroyInternal(): Promise<CleanupRecord> {
    this.destroyed = true
    if (this.eventPump !== null) {
      clearTimeout(this.eventPump)
      this.eventPump = null
    }
    await this.eventPumpRunning
    for (const subscription of this.subscriptions.values()) {
      subscription.closed = true
      subscription.notifications.closeWithReason('owner-released')
    }
    this.subscriptions.clear()
    const group = this.scanGroup
    this.scanGroup = null
    if (group !== null) {
      for (const consumer of group.consumers.values()) {
        consumer.removeAbort()
        consumer.stream.closeWithReason('owner-released')
      }
      group.consumers.clear()
    }
    for (const stream of this.adapterTransitions) stream.closeWithReason('owner-released')
    this.adapterTransitions.clear()
    this.security?.close?.()
    for (const watch of [...this.readinessWatches]) this.closeReadinessWatch(watch, 'owner-released')
    for (const watch of [...this.parameterWatches]) this.closeParameterWatch(watch, 'owner-released')
    // The core's shutdown stops the scan, releases every link and CCCD, and
    // reports each release failure; that report is the cleanup record.
    let report
    try {
      report = await this.central.close()
    } catch (error) {
      throwDesktopRustCoreError(error, this.op('session.dispose'))
    } finally {
      await Promise.all([...this.eventStreams].map(stream => stream.close()))
      this.eventStreams.clear()
    }
    return cleanupRecordFromCloseReport(report, this.op('session.dispose'))
  }

  private op(name: string): string {
    return desktopRustCoreOperation(this.profile.operationPrefix, name)
  }

  private assertAlive(operation: string): void {
    if (this.destroyed) {
      throw contractError('lifecycle.destroyed', 'core', operation)
    }
  }

  /**
   * Radio admission here is lifecycle only: adapter power and permission
   * refusals (`adapter.powered-off`, `permission.denied`, ...) come from the
   * core's own per-OS admission, before any radio effect (LEGACY-AUDIT-1 #58).
   */
  private assertOperational(operation: string): void {
    this.assertAlive(operation)
  }

  private nextOrdinal(): number {
    const ordinal = this.nextOrdinalValue
    this.nextOrdinalValue += 1
    return ordinal
  }

  private nextEventOrdinal(): number {
    const ordinal = this.nextEventOrdinalValue
    this.nextEventOrdinalValue += 1
    return ordinal
  }

  private emitEvent(event: BackendEvent<string>): void {
    for (const stream of [...this.eventStreams]) {
      if (stream.emit(event, 128).terminated) this.eventStreams.delete(stream)
    }
  }

  /**
   * A fact the typed surface cannot express rides the event stream as a
   * diagnostic warning: a failed background cleanup is never silent.
   */
  private noteDiagnostic(code: string, message: string, detail: SerializableRecord = Object.freeze({})): void {
    if (this.eventStreams.size === 0) return
    this.emitEvent({
      kind: 'diagnostic-warning',
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      ingressOrdinal: this.nextEventOrdinal(),
      code,
      message: message.slice(0, 512),
      detail
    })
  }

  private noteBackgroundFailure(code: string, error: unknown): void {
    const normalized = desktopRustCoreError(error, this.op(code)).normalized
    this.noteDiagnostic(code, `${normalized.code} at ${normalized.operation}`, {
      code: normalized.code,
      operation: normalized.operation,
      detail: normalized.platform?.safeMessage ?? null
    })
  }

  /**
   * The caller deadline as a relative core budget. A null deadline means the
   * caller set no bound: nothing crosses, and the core's named liveness
   * backstop bounds the operation. An expired deadline crosses as zero so
   * the core refuses it before any radio call.
   */
  private budget(options: PublicOperationOptions): { timeoutMs?: number } {
    if (options.deadline === null || options.deadline === undefined) return {}
    const remaining = Math.floor(Number(options.deadline) - this.now())
    if (!Number.isFinite(remaining)) return {}
    return { timeoutMs: Math.min(Math.max(0, remaining), 0xffffffff) }
  }

  /**
   * Run one core call under a cancellation ticket. The ticket exists before
   * the call is issued, so an abort racing the call is recorded by the core
   * (the op ends `operation.aborted` without a radio call) and an abort after
   * admission cancels exactly that core operation. An already-aborted signal
   * refuses before any dispatch, carrying the bare operation id as the
   * legacy dispatcher reported it (F4, no `.aborted` suffix).
   */
  private async withTicket<Result>(
    correlation: string,
    signal: AbortSignal | null | undefined,
    operation: string,
    run: (ticket: string) => Promise<Result>,
    gattPeerId?: string
  ): Promise<Result> {
    if (signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', operation)
    }
    let ticket: string
    try {
      ticket = this.central.createTicket(gattPeerId)
    } catch (error) {
      throwDesktopRustCoreError(error, `${operation}.ticket`)
    }
    this.ticketsByCorrelation.set(correlation, ticket)
    const onAbort = (): void => {
      this.central.cancelTicket(ticket).catch(error => this.noteBackgroundFailure('cancel-ticket-failed', error))
    }
    signal?.addEventListener('abort', onAbort, { once: true })
    this.dispatchedOperations += 1
    try {
      return await run(ticket)
    } catch (error) {
      throw desktopRustCoreError(error, operation)
    } finally {
      this.dispatchedOperations -= 1
      signal?.removeEventListener('abort', onAbort)
      this.ticketsByCorrelation.delete(correlation)
      try {
        this.central.releaseTicket(ticket)
      } catch (error) {
        this.noteBackgroundFailure('release-ticket-failed', error)
      }
    }
  }

  private async requestCancellation(correlation: string): Promise<CancellationAcknowledgement<string>> {
    const handle = this.identifiers.backendOperationHandle(correlation)
    const ticket = this.ticketsByCorrelation.get(correlation)
    if (ticket === undefined) {
      return { handle, state: 'already-terminal' }
    }
    try {
      const ack = await this.central.cancelTicket(ticket)
      return { handle, state: ack.outcome === 'already-settled' ? 'already-terminal' : 'cancellation-requested' }
    } catch (error) {
      throwDesktopRustCoreError(error, this.op('operation.cancel'))
    }
  }

  /**
   * Admit one GATT verb on a connection (F6): CoreBluetooth serializes GATT
   * verbs per connection — a second concurrent verb fails fast with
   * `lifecycle.invalid-state` under the verb's own operation id, as the
   * legacy dispatcher refused it. Other radios admit concurrent verbs; the
   * radio itself refuses same-characteristic races.
   */
  private admitGattVerb(connectionId: string, verb: string): void {
    if (this.profile.platform !== 'corebluetooth') return
    if (this.gattVerbsInFlight.has(connectionId)) {
      throw contractError('lifecycle.invalid-state', 'core', this.op(verb))
    }
    this.gattVerbsInFlight.add(connectionId)
  }

  /** Release the per-connection GATT verb slot when its completion settles. */
  private trackGattVerb<Result>(connectionId: string, completion: Promise<Result>): Promise<Result> {
    if (this.profile.platform !== 'corebluetooth') return completion
    const release = (): void => {
      this.gattVerbsInFlight.delete(connectionId)
    }
    return completion.then(
      value => {
        release()
        return value
      },
      error => {
        release()
        throw error
      }
    )
  }

  private dispatchFor<Result>(
    correlation: string,
    completion: Promise<Result>
  ): BackendOperationDispatch<string, Result> {
    const handle = this.identifiers.backendOperationHandle(correlation)
    return createBackendOperationDispatch<string, Result>(handle, completion, () =>
      this.requestCancellation(correlation)
    )
  }

  private succeededTerminal(
    correlation: OperationCorrelation<string, string>
  ): OperationTerminalRecord<string, string> {
    return Object.freeze({ correlation, outcome: 'succeeded', cause: null })
  }

  /**
   * A provider-minted operation correlation, in the legacy shape
   * `operation-{n}` (the core's `CoreOperationCoordinator` mints the same
   * shape for caller-driven operations): the kind of operation is carried
   * by the operation id, never the correlation.
   */
  private mintedCorrelation(): OperationCorrelation<string, string> {
    return this.identifiers.operationCorrelation(`operation-${this.nextOrdinal()}`)
  }

  // -- adapter -------------------------------------------------------------

  /**
   * The adapter state after the core opened a central on it: the adapter is
   * `available` (the OS handed the core an adapter) unless the OS reports
   * Bluetooth LE unsupported, which is the legacy CoreBluetooth mapping:
   * availability and power `unsupported`, authorization `unavailable`.
   * Power is the OS's answer, or `unknown` with the reason when the radio
   * cannot read it.
   */
  private snapshotFor(
    power: DesktopRustCoreAdapterPower | null,
    authorization: DesktopRustCoreAdapterAuthorization | null,
    reason: string | null
  ): AdapterStateSnapshot<string> {
    if (power === 'unsupported') {
      return Object.freeze({
        availability: 'unsupported' as const,
        authorization: 'unavailable' as const,
        power: 'unsupported' as const,
        backendGeneration: this.attachment.backendGeneration,
        updatedAt: monotonicTimestamp(this.now()),
        safeReason: reason ?? 'the OS reports Bluetooth LE is unsupported on this host'
      })
    }
    const measuredPower = adapterPower(power)
    // CoreBluetooth's Unauthorized is the legacy `authorization: 'denied'`.
    const measuredAuthorization = power === 'unauthorized' ? 'denied' : (authorization ?? ('unknown' as const))
    return Object.freeze({
      availability: 'available' as const,
      authorization: measuredAuthorization,
      power: measuredPower,
      backendGeneration: this.attachment.backendGeneration,
      updatedAt: monotonicTimestamp(this.now()),
      safeReason:
        reason ??
        (power === 'unauthorized'
          ? 'the OS denies this process the Bluetooth adapter'
          : authorization === null
            ? this.profile.authorizationReason
            : 'measured by the OS')
    })
  }

  /**
   * The adapter power as the OS reports it. A radio that cannot read power
   * (`capability.unsupported`) reports `unknown` with that reason; any other
   * failure propagates — an adapter that cannot answer is not "available".
   */
  private async measureAdapterState(operation: string): Promise<AdapterStateSnapshot<string>> {
    let power: DesktopRustCoreAdapterPower | null
    let reason: string | null = null
    try {
      power = await this.central.adapterState()
    } catch (error) {
      const normalized = desktopRustCoreError(error, operation)
      if (normalized.normalized.code !== 'capability.unsupported') throw normalized
      power = null
      reason = `adapter power is not readable on this radio: ${normalized.normalized.platform?.safeMessage ?? 'unsupported'}`
    }
    const authorization = await this.measureAuthorization(operation)
    return this.snapshotFor(power, authorization, reason)
  }

  /**
   * Whether the OS lets this process use the adapter. A platform without the
   * concept answers `capability.unsupported`: `unknown` with the reason,
   * never a denial.
   */
  private async measureAuthorization(operation: string): Promise<DesktopRustCoreAdapterAuthorization | null> {
    try {
      return await this.central.adapterAuthorization()
    } catch (error) {
      const normalized = desktopRustCoreError(error, `${operation}.authorization`)
      if (normalized.normalized.code === 'capability.unsupported') return null
      throw normalized
    }
  }

  private async currentAdapterState(): Promise<AdapterStateSnapshot<string>> {
    this.assertAlive(this.op('adapter.state'))
    if (!this.opened) {
      throw contractError('lifecycle.invalid-state', 'core', this.op('adapter.state-before-open'))
    }
    this.adapterState = await this.measureAdapterState(this.op('adapter.state'))
    return this.adapterState
  }

  private async watchAdapterState(): Promise<AdapterStateWatch<string>> {
    const initial = await this.currentAdapterState()
    const transitions = new CoreBoundedStream<AdapterStateSnapshot<string>>(
      // F10: legacy adapter-state quotas (16/16KiB/1).
      { itemCapacity: capacity(16), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
      'latest'
    )
    this.adapterTransitions.add(transitions)
    return Object.freeze({ initial, transitions })
  }

  // -- core events -----------------------------------------------------------

  /**
   * Lifecycle and adapter events are polled from the core on an unref'd
   * timer: they are auxiliary to the host's own work, so an idle backend
   * never holds the process open by itself (the pumps behind live scans and
   * subscriptions do, deliberately).
   */
  private scheduleEventPump(): void {
    if (this.destroyed || this.coreEventsClosed) return
    const timer = setTimeout(() => {
      this.eventPump = null
      this.runEventDrain()
    }, EVENT_PUMP_INTERVAL_MS)
    timer.unref?.()
    this.eventPump = timer
  }

  /** One drain turn; a wake that arrived during it runs another at once. */
  private runEventDrain(): Promise<void> {
    const turn = this.drainCoreEvents().finally(() => {
      this.eventPumpRunning = null
      if (this.drainRequested) {
        this.drainRequested = false
        this.requestEventDrain()
      } else {
        this.scheduleEventPump()
      }
    })
    this.eventPumpRunning = turn
    return turn
  }

  /**
   * The addon reported new work (the event waker): every pump polls now and
   * the core events drain now, rather than on their next interval — the
   * legacy backends delivered by callback (LEGACY-AUDIT-4 R2). The core
   * wakes for every report kind (finding 118); the intervals remain only as
   * a safety net.
   */
  private onCoreWake(): void {
    this.wakePumps()
    this.requestEventDrain()
  }

  private requestEventDrain(): void {
    if (this.destroyed || this.coreEventsClosed) return
    if (this.eventPumpRunning !== null) {
      this.drainRequested = true
      return
    }
    if (this.eventPump !== null) {
      clearTimeout(this.eventPump)
      this.eventPump = null
    }
    this.runEventDrain()
  }

  /**
   * Apply every core event queued now (lifecycle, security, readiness, scan
   * terminal, adapter, reset) instead of waiting for the next pump turn:
   * a host that just caused an OS event (a deterministic harness, a TCK
   * controller) settles the backend's view before it looks. Runs after any
   * turn already in flight, never beside it, so events keep their order.
   */
  async settleCoreEvents(): Promise<void> {
    while (this.eventPumpRunning !== null) await this.eventPumpRunning
    if (this.destroyed) return
    if (this.eventPump !== null) {
      clearTimeout(this.eventPump)
      this.eventPump = null
    }
    await this.runEventDrain()
  }

  private async drainCoreEvents(): Promise<void> {
    try {
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const lifecycle = await this.central.takeLifecycleEvent()
        if (lifecycle === null || lifecycle === undefined) break
        if (lifecycle.kind === 'lagged') await this.reconcileLinks('lifecycle', lifecycle.missed ?? null)
        else this.applyLifecycleEvent(lifecycle)
      }
      if (typeof this.central.takeSourceUncertaintyEvent === 'function') {
        for (;;) {
          if (this.destroyed || this.coreEventsClosed) return
          const event = await this.central.takeSourceUncertaintyEvent()
          if (event === null || event === undefined) break
          this.applySourceUncertaintyEvent(event)
        }
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const security = await this.central.takeSecurityEvent()
        if (security === null || security === undefined) break
        if (security.kind === 'lagged') await this.reconcileSecurity(security.missed ?? null)
        else this.applySecurityEvent(security)
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const readiness = await this.central.takeWriteReadinessEvent()
        if (readiness === null || readiness === undefined) break
        if (readiness.kind === 'lagged') await this.reconcileWriteReadiness(readiness.missed ?? null)
        else this.applyWriteReadiness(readiness)
      }
      if (this.wiring.connectionParameters && typeof this.central.takeConnectionParameterEvent === 'function') {
        for (;;) {
          if (this.destroyed || this.coreEventsClosed) return
          const parameters = await this.central.takeConnectionParameterEvent()
          if (parameters === null || parameters === undefined) break
          if (parameters.kind === 'lagged') await this.reconcileConnectionParameters(parameters.missed ?? null)
          else this.applyConnectionParameters(parameters)
        }
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const terminal = await this.central.takeScanTerminalEvent()
        if (terminal === null || terminal === undefined) break
        if (terminal.kind === 'lagged') this.reconcileScan(terminal.missed ?? null)
        else await this.applyScanTerminal(terminal)
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        // Adapter states and reset teardown arrive on separate core queues.
        // A reset is a boundary for the state event named by its
        // `adapterSequence`, so take it before admitting a later recovery
        // state. Draining every state first could otherwise publish off → on
        // on the old generation before the reset is applied.
        const reset = await this.central.takeAdapterResetEvent()
        if (reset !== null && reset !== undefined) {
          if (reset.kind === 'lagged') await this.reconcileAdapterReset(reset.missed ?? null)
          else this.applyAdapterReset(reset)
          continue
        }
        const adapter = await this.central.takeAdapterEvent()
        if (adapter === null || adapter === undefined) break
        if (adapter.kind === 'state' && typeof adapter.state === 'string') {
          // A reset may already have applied this state from its own causal
          // record. Never replay that state, or an older one, after the
          // generation boundary. Gap and closure markers still need their
          // own handling below.
          if (typeof adapter.sequence === 'number' && adapter.sequence <= this.adapterStateSequence) continue
          this.applyAdapterPower(adapter.state)
          if (typeof adapter.sequence === 'number') {
            this.adapterStateSequence = Math.max(this.adapterStateSequence, adapter.sequence)
          }
        } else if (adapter.kind === 'lagged') {
          this.noteDiagnostic('adapter-events-lagged', 'adapter events were missed; re-reading adapter state', {
            missed: adapter.missed ?? null
          })
          this.applyAdapterPower(await this.central.adapterState())
        } else if (adapter.kind === 'closed') {
          this.failCoreEventSource('adapter-events-closed')
        }
      }
    } catch (error) {
      if (this.destroyed || this.coreEventsClosed) return
      this.noteBackgroundFailure('core-event-pump-failed', error)
      await this.reconcileAfterPumpFailure()
    }
  }

  /**
   * A failed pump turn may have taken core events it never applied. Re-read
   * every fact the core can answer, as a lag of unknown size would: live
   * links, security and write-readiness watches, the owned scan and the
   * adapter power. A missed adapter reset still shows here, as the links and
   * scan the core no longer holds.
   */
  private async reconcileAfterPumpFailure(): Promise<void> {
    try {
      await this.reconcileLinks('lifecycle', null)
      await this.reconcileSecurity(null)
      await this.reconcileWriteReadiness(null)
      if (this.wiring.connectionParameters) await this.reconcileConnectionParameters(null)
      this.reconcileScan(null)
      if (!this.destroyed) this.applyAdapterPower(await this.central.adapterState())
    } catch (error) {
      if (!this.destroyed) this.noteBackgroundFailure('core-event-reconcile-failed', error)
    }
  }

  /**
   * The core closed one of its event streams while this backend is live: the
   * central that publishes them is gone, and no later lifecycle, security,
   * readiness, scan-terminal or adapter fact can arrive. The backend event
   * source fails (`source-failed`), so the manager releases everything it
   * holds rather than serving handles no event will ever end.
   */
  private failCoreEventSource(code: string): void {
    if (this.destroyed || this.coreEventsClosed) return
    this.noteDiagnostic(code, 'the core closed an event stream; its facts can no longer arrive')
    this.coreEventsClosed = true
    const error = this.coreEventsClosedError()
    for (const stream of [...this.eventStreams]) stream.closeWithReason('source-failed', error)
    this.eventStreams.clear()
  }

  private coreEventsClosedError(): NormalizedBleError {
    return contractError('platform.failure', 'core', this.op('events.closed')).normalized
  }

  /**
   * One link the core released because the adapter was lost: its streams
   * end `source-failed` and its databases go stale; CoreBluetooth and WinRT
   * announce `connection-state-changed` with reason `adapter`, BlueZ ends it
   * silently (the legacy per-OS sequence, `ADAPTER_LOSS_SEQUENCE`).
   *
   * The transition's `previous` is the last state announced to the core, which
   * this provider only ever tells of terminal transitions: `connected`, also
   * while the caller's own release is in flight. `disconnecting` is the
   * provider's private marker for that release; announcing it breaks the
   * core's previous-state invariant and ends the whole manager
   * `backend-failure` instead of ending this link `adapter-loss`.
   */
  private applyAdapterLostLink(record: ConnectionRecord, hostReset = false, platform?: PlatformErrorDetail): void {
    if (record.state !== 'connected' && record.state !== 'disconnecting') return
    record.state = 'lost'
    for (const subscriptionId of record.subscriptions) {
      const subscription = this.subscriptions.get(subscriptionId)
      if (subscription === undefined) continue
      this.endSubscriptionAfterDrain(subscription, 'source-failed')
      this.subscriptions.delete(subscriptionId)
    }
    record.subscriptions.clear()
    for (const databaseId of record.databases) this.databases.delete(databaseId)
    record.databases.clear()
    for (const watch of [...this.readinessWatches]) {
      if (watch.record !== record) continue
      this.closeReadinessWatch(watch, 'connection-lost')
    }
    for (const watch of [...this.parameterWatches]) {
      if (watch.record !== record) continue
      this.closeParameterWatch(watch, 'connection-lost')
    }
    if (hostReset || ADAPTER_LOSS_SEQUENCE[this.profile.platform].connectionStateChanged) {
      this.emitEvent({
        kind: 'connection-state-changed',
        attachment: this.attachment,
        attachmentId: this.attachment.attachmentId,
        ingressOrdinal: this.nextEventOrdinal(),
        connection: record.path,
        previous: 'connected',
        current: 'lost',
        reason: hostReset ? 'backend-restart' : 'adapter',
        ...(platform === undefined ? {} : { platform })
      })
    }
  }

  /**
   * The core finished an adapter-loss teardown (power-off, resetting,
   * unsupported, unauthorized, adapter removed, daemon restarted): every
   * live operation settled `operation.reset`, the scan ended, links and
   * subscriptions released. Anything its individual events have not yet
   * ended here ends now, then the backend generation advances, stale peer
   * handles are dropped and CoreBluetooth / BlueZ announce
   * `backend-restarted` (the legacy per-OS sequence).
   */
  private applyAdapterReset(event: DesktopRustCoreAdapterResetEvent): void {
    if (event.kind === 'closed') {
      this.failCoreEventSource('adapter-reset-events-closed')
      return
    }
    if (event.kind === 'lagged') {
      throw contractError('lifecycle.invariant-violation', 'core', this.op('adapter-reset.lagged-unreconciled'))
    }
    const released = new Set(event.releasedLinks ?? [])
    for (const record of this.connectionsById.values()) {
      if (released.has(record.nativePeerId)) {
        const hostReset = event.cause === 'source-observation-overflow'
        this.applyAdapterLostLink(
          record,
          hostReset,
          hostReset
            ? {
                domain: 'ubm',
                code: 'source-observation-overflow',
                safeMessage: 'Host observation capacity exceeded',
                metadata: {}
              }
            : undefined
        )
      }
    }
    const group = this.scanGroup
    // The core ends every scan it owns on a reset; its aborted scan
    // terminal may not have been drained yet.
    if (group !== null) {
      this.scanGroup = null
      group.state = 'stopped'
      for (const consumer of group.consumers.values()) {
        consumer.removeAbort()
        consumer.stream.closeWithReason('source-failed')
      }
      group.consumers.clear()
    }
    for (const failure of event.releaseFailures ?? []) {
      this.noteBackgroundFailure('adapter-reset-release-failed', new Error(failure))
    }
    // The reset carries the state that caused it and the matching adapter
    // event sequence. This is the ordering fact: it tells us whether that
    // state event is still queued without taking a second, potentially later
    // adapter-status observation.
    const causalAdapterSequence = event.adapterSequence
    const adapterEventPending =
      typeof causalAdapterSequence === 'number' && causalAdapterSequence > this.adapterStateSequence
    this.advanceGeneration(
      event.cause === 'source-observation-overflow' || ADAPTER_LOSS_SEQUENCE[this.profile.platform].backendRestarted,
      adapterEventPending
    )
    if (adapterEventPending && event.power !== null && event.power !== undefined) {
      // `advanceGeneration` has already minted the new generation, so an
      // equal causal power still must publish that generation exactly once.
      // The matching queued adapter event is suppressed by its sequence.
      this.applyAdapterPower(event.power, true)
      this.adapterStateSequence = causalAdapterSequence
    }
  }

  /**
   * Adapter resets were missed (LEGACY-AUDIT-2 N5). A missed reset still
   * advanced the core, which released every link and ended its scan: the
   * links the core no longer holds end as the reset ended them, a scan it
   * no longer owns ends `source-failed`, and this generation ends too
   * rather than serving handles from before the reset.
   */
  private async reconcileAdapterReset(missed: number | null): Promise<void> {
    this.noteDiagnostic('adapter-reset-events-lagged', 'adapter resets were missed; re-reading links and scan', {
      missed
    })
    await this.reconcileLinks('adapter-reset', missed)
    this.reconcileScan(missed)
    if (this.destroyed) return
    this.advanceGeneration(ADAPTER_LOSS_SEQUENCE[this.profile.platform].backendRestarted)
  }

  /**
   * Lifecycle events were missed (LEGACY-AUDIT-2 N5): legacy delivered each
   * transition by direct callback, so a gap here must not leave a lost link
   * reported connected. Re-read every live link from the core itself — its
   * connection state and generation, and its database state — and emit the
   * transitions the missed events would have: a link the core no longer
   * holds (or holds under another generation) ends as lost (or, while the
   * caller's own release is in flight, as released; while the adapter is
   * lost or on an adapter-reset gap, as the loss ended it), and a database the core no longer reports
   * current is `database-changed`. A link the core still reports live and
   * current is left alone: nothing is inferred.
   */
  private async reconcileLinks(cause: 'lifecycle' | 'adapter-reset', missed: number | null): Promise<void> {
    if (cause === 'lifecycle') {
      this.noteDiagnostic('lifecycle-events-lagged', 'connection lifecycle events were missed; re-reading links', {
        missed
      })
    }
    const live = (): ConnectionRecord[] =>
      [...this.connectionsById.values()].filter(
        record => record.state === 'connected' || record.state === 'disconnecting'
      )
    if (live().length === 0) return
    const peers = new Map((await this.central.peerRecords()).map(peer => [peer.peerId, peer]))
    // A missed `adapter-lost` lifecycle event reads as a link the core
    // released while the adapter is lost: it ends the way the loss ends it.
    const adapterLost = cause === 'adapter-reset' || this.central.adapterStatus().lost
    for (const record of live()) {
      if (this.destroyed) return
      const peer = peers.get(record.nativePeerId)
      const linkLive =
        peer !== undefined &&
        (peer.connectionState === 'connected' || peer.connectionState === 'disconnecting') &&
        (typeof peer.connectionGeneration !== 'string' || peer.connectionGeneration === record.coreGeneration)
      if (!linkLive) {
        if (adapterLost) {
          this.applyAdapterLostLink(record)
          continue
        }
        this.applyLifecycleEvent({
          kind: record.state === 'disconnecting' ? 'released' : 'link-lost',
          peerId: record.nativePeerId,
          connectionGeneration: record.coreGeneration,
          requested: record.state === 'disconnecting'
        })
        continue
      }
      if (record.databases.size > 0 && peer.databaseState !== 'current') {
        this.applyLifecycleEvent({
          kind: 'services-changed',
          peerId: record.nativePeerId,
          connectionGeneration: record.coreGeneration
        })
      }
    }
  }

  /** A new backend generation: new attachment, stale peer handles dropped, and watchers told. */
  private advanceGeneration(announceRestart: boolean, adapterEventPending = false): void {
    this.generation += 1
    this.attachment = this.attachmentFor(this.attachment.adapter, this.adapterState)
    this.identifiers = this.identifiersFor(this.attachment)
    this.adapterState = Object.freeze({ ...this.adapterState, backendGeneration: this.attachment.backendGeneration })
    // 5.0 keeps peer handles (and their address facts) across the advance:
    // the peer is the same device, so a supervisor reconnects with the handle
    // it holds. Legacy cleared them with the generation and ended the manager.
    for (const streams of this.securityWatches.values()) {
      for (const stream of streams) stream.closeWithReason('source-failed')
    }
    this.securityWatches.clear()
    // A causal state event still queued behind this reset publishes the new
    // generation itself. Otherwise the generation advance is observable now.
    if (!adapterEventPending) {
      for (const stream of [...this.adapterTransitions]) {
        if (stream.emit(this.adapterState, 96).terminated) this.adapterTransitions.delete(stream)
      }
    }
    if (announceRestart) {
      this.emitEvent({
        kind: 'backend-restarted',
        attachment: this.attachment,
        attachmentId: this.attachment.attachmentId,
        ingressOrdinal: this.nextEventOrdinal()
      })
    }
  }

  private applyAdapterPower(power: DesktopRustCoreAdapterPower, forceTransition = false): void {
    if (this.destroyed) return
    // A power change keeps the last measured authorization and its reason,
    // except where the state itself decides them (unsupported).
    const snapshot = this.snapshotFor(power, null, null)
    const previous = this.adapterState
    this.adapterState =
      power === 'unsupported' || power === 'unauthorized'
        ? snapshot
        : Object.freeze({
            ...snapshot,
            authorization: this.adapterState.authorization,
            safeReason: this.adapterState.safeReason
          })
    // A re-announced snapshot that changes nothing observable is not a
    // transition: emitting it would duplicate the watch's initial snapshot
    // (or an earlier transition) for every subscriber.
    if (forceTransition || !sameObservableAdapterState(previous, this.adapterState)) {
      for (const stream of [...this.adapterTransitions]) {
        if (stream.emit(this.adapterState, 96).terminated) this.adapterTransitions.delete(stream)
      }
    }
    this.emitEvent({
      kind: 'adapter-state',
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      ingressOrdinal: this.nextEventOrdinal()
    })
  }

  /**
   * Every live record of the event's link: leases share one native link, so
   * a link-level event reaches each lease's record, never just the first.
   */
  private connectionFor(event: DesktopRustCoreLifecycleEvent): ConnectionRecord[] {
    const matched: ConnectionRecord[] = []
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId !== event.peerId) continue
      if (record.state !== 'connected' && record.state !== 'disconnecting') continue
      if (typeof event.connectionGeneration === 'string' && event.connectionGeneration !== record.coreGeneration)
        continue
      matched.push(record)
    }
    return matched
  }

  /**
   * Map one core lifecycle event onto the contract events the legacy
   * backends emitted: a link lost without a release request is
   * `connection-lost`; a GATT database change is `database-changed` for
   * every database of that connection; a requested release is the caller's
   * own disconnect, already settled through its cleanup record. Streams the
   * event ends close with the matching terminal reason.
   */
  private applyLifecycleEvent(event: DesktopRustCoreLifecycleEvent): void {
    if (event.kind === 'lagged') {
      throw contractError('lifecycle.invariant-violation', 'core', this.op('lifecycle.lagged-unreconciled'))
    }
    if (event.kind === 'closed') {
      this.failCoreEventSource('lifecycle-events-closed')
      return
    }
    const records = this.connectionFor(event)
    if (records.length === 0) return
    if (event.kind === 'adapter-lost' || event.kind === 'host-reset') {
      const platform = parseDesktopRustCoreLifecyclePlatform(event.platform)
      for (const record of records) this.applyAdapterLostLink(record, event.kind === 'host-reset', platform)
      return
    }
    if (event.kind === 'services-changed') {
      for (const record of records) {
        this.invalidateConnectionState(record, 'service-changed')
        for (const databaseId of [...record.databases]) {
          const database = this.databases.get(databaseId)
          this.databases.delete(databaseId)
          record.databases.delete(databaseId)
          if (database === undefined) continue
          this.emitEvent({
            kind: 'database-changed',
            attachment: this.attachment,
            attachmentId: this.attachment.attachmentId,
            ingressOrdinal: this.nextEventOrdinal(),
            database: database.base
          })
        }
      }
      return
    }
    const requested = event.kind === 'released' && event.requested === true
    const platform = parseDesktopRustCoreLifecyclePlatform(event.platform)
    for (const record of records) {
      record.state = requested ? 'disconnected' : 'lost'
      this.invalidateConnectionState(record, 'connection-lost')
      if (requested) {
        record.releasePlatform = platform
        if (record.requestedDisconnect === true) this.emitRequestedDisconnect(record)
        continue
      }
      this.emitEvent({
        kind: 'connection-lost',
        attachment: this.attachment,
        attachmentId: this.attachment.attachmentId,
        ingressOrdinal: this.nextEventOrdinal(),
        connection: record.path,
        ...(platform === undefined ? {} : { platform })
      })
    }
  }

  /**
   * Surface a native callback which had no accepted graph as an ownerless
   * diagnostic. It must never be converted into a lifecycle event, because
   * the identifier may have been reused by a newer connection.
   */
  private applySourceUncertaintyEvent(event: DesktopRustCoreSourceUncertaintyEvent): void {
    if (event.kind === 'closed') return
    if (event.kind === 'lagged') {
      this.noteDiagnostic('gatt.source-uncertain-lagged', 'ownerless native source diagnostics were lagged', {
        missed: event.missed ?? 0
      })
      return
    }
    if (event.kind !== 'source-uncertain') {
      throw contractError('protocol.malformed', 'boundary', this.op('source-uncertainty.kind'))
    }
    if (typeof event.peerId !== 'string' || typeof event.code !== 'string' || typeof event.operation !== 'string') {
      throw contractError('protocol.malformed', 'boundary', this.op('source-uncertainty.event'))
    }
    const platform = parseDesktopRustCoreLifecyclePlatform(event.platform)
    this.noteDiagnostic('gatt.source-uncertain', 'native source could not be associated with an accepted graph', {
      peerId: event.peerId,
      code: event.code,
      operation: event.operation,
      ...(typeof event.detail === 'string' ? { detail: event.detail } : {}),
      ...(platform === undefined
        ? {}
        : {
            platform: Object.freeze({
              domain: platform.domain,
              code: platform.code,
              message: platform.safeMessage,
              metadata: snapshotSerializableRecord(platform.metadata).value
            })
          })
    })
  }

  private invalidateConnectionState(record: ConnectionRecord, reason: 'connection-lost' | 'service-changed'): void {
    for (const subscriptionId of [...record.subscriptions]) {
      const subscription = this.subscriptions.get(subscriptionId)
      if (subscription === undefined) continue
      this.endSubscriptionAfterDrain(subscription, reason)
    }
    if (reason === 'connection-lost') {
      for (const databaseId of record.databases) this.databases.delete(databaseId)
      record.databases.clear()
      for (const watch of [...this.parameterWatches]) {
        if (watch.record === record) this.closeParameterWatch(watch, 'connection-lost')
      }
      for (const watch of [...this.readinessWatches]) {
        if (watch.record === record) this.closeReadinessWatch(watch, 'connection-lost')
      }
    }
  }

  private emitRequestedDisconnect(record: ConnectionRecord): void {
    if (record.requestedTerminalAnnounced === true) return
    record.requestedTerminalAnnounced = true
    this.emitEvent({
      kind: 'disconnected',
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      ingressOrdinal: this.nextEventOrdinal(),
      connection: record.path,
      reason: 'local',
      ...(record.releasePlatform === undefined ? {} : { platform: record.releasePlatform })
    })
  }

  // -- peers ---------------------------------------------------------------

  private assertPeerQueryUsable(operation: string, options: BackendPeerQuery): void {
    this.assertOperational(operation)
    if (options.signal?.aborted === true) throw contractError('operation.aborted', 'core', operation)
    if (options.deadline !== null && options.deadline !== undefined && options.deadline <= this.now()) {
      throw contractError('operation.timed-out', 'core', operation)
    }
  }

  private async runPeerQuery<Result>(
    operation: string,
    options: BackendPeerQuery,
    read: (control: DesktopRustCoreControl) => Promise<Result>
  ): Promise<Result> {
    this.assertPeerQueryUsable(operation, options)
    const generation = this.generation
    const result = await this.withTicket(`peer-query-${this.nextOrdinal()}`, options.signal, operation, ticket =>
      read({ ticket, ...this.budget(options) })
    )
    // A read-only lookup may finish after cancellation, destruction or an
    // adapter reset. Retain observation, but never publish a stale mapping.
    this.assertPeerQueryUsable(operation, options)
    if (generation !== this.generation) throw contractError('lifecycle.invalid-state', 'core', operation)
    return result
  }

  private peerIdForNativeId(nativePeerId: string): PeerId<string> {
    const existing = this.peerIdsByNativeId.get(nativePeerId)
    if (existing !== undefined) return existing
    const peerId = this.mintPeer()
    this.peerIdsByNativeId.set(nativePeerId, peerId)
    this.nativeIdsByPeerId.set(String(peerId), nativePeerId)
    return peerId
  }

  /** The next legacy peer name of this backend. */
  private mintPeer(): PeerId<string> {
    const ordinal = this.nextPeer
    this.nextPeer += 1
    return opaqueId(
      LEGACY_DESKTOP_NAMES[this.profile.platform].peer(this.generation, ordinal),
      'peer',
      `${this.profile.platform}-rust-core`
    )
  }

  private mintScanOrdinal(): number {
    const ordinal = this.nextScan
    this.nextScan += 1
    return ordinal
  }

  // -- scan ------------------------------------------------------------------

  private planScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
    return createServiceUuidScanPlan(
      query,
      {
        backendId: this.profile.backendId,
        platformId: this.profile.platformId,
        availableObservationFields: this.profile.observationFields
      },
      `invalid ${this.profile.platform} rust-core scan planning context`
    )
  }

  /**
   * The native filter: the planned service UUIDs when the caller supplied a
   * trusted plan (the public manager), else the caller's own filter. Service
   * UUIDs and a local-name prefix reach the OS (btleplug's ScanFilter; the
   * prefix becomes BlueZ's `Pattern`, as legacy BlueZ sent it); every
   * predicate still runs in the canonical software matcher, as the legacy
   * backends did.
   */
  private scanFilterFor(options: OwnerScanOptions<string, string>, operation: string): ScanFilter {
    assertScanFilter(options.filter, operation)
    if (options.platform !== undefined && (this.profile.platform !== 'winrt' || options.platform.kind !== 'winrt')) {
      throw contractError('capability.unsupported', 'scan', `${operation}.platform-options`)
    }
    if (options.platform?.kind === 'winrt')
      decodeWinRtScanPlatformOptions(options.platform, `${operation}.platform-options`)
    return trustedServiceUuidFilter(options, query => this.planScan(query), operation)
  }

  private hasTerminalEvidenceSurface(): boolean {
    return (
      this.central.reserveTerminalEvidence !== undefined &&
      this.central.commitTerminalEvidence !== undefined &&
      this.central.readTerminalEvidence !== undefined &&
      this.central.releaseTerminalEvidence !== undefined &&
      this.central.stageTerminalEvidence !== undefined &&
      this.central.acknowledgeRecoveredTerminalEvidence !== undefined
    )
  }

  /** Whether recovery may contain host-authorized durable records. */
  hasTerminalEvidenceRecovery(): boolean {
    return this.terminalEvidenceConfigured
  }

  /**
   * This native ledger reservation is deliberately made before source
   * activation. A source receives only the exact opaque scope, not a path or
   * a JavaScript fallback store.
   */
  private async reserveTerminalEvidence(
    request: TerminalEvidenceReservationRequest
  ): Promise<TerminalEvidenceReservation> {
    const reserve = this.central.reserveTerminalEvidence
    const commit = this.central.commitTerminalEvidence
    const release = this.central.releaseTerminalEvidence
    const stage = this.central.stageTerminalEvidence
    if (reserve === undefined || commit === undefined || release === undefined || stage === undefined) {
      throw contractError('capability.unsupported', 'stream', this.op('terminal-evidence.reserve'))
    }
    let reserved
    try {
      reserved = await reserve.call(this.central, {
        producer: request.producer,
        maximumBytes: String(request.maximumBytes),
        formatVersion: request.formatVersion,
        scope: request.scope
      })
    } catch (error) {
      throwDesktopRustCoreError(error, this.op('terminal-evidence.reserve'))
    }
    if (
      typeof reserved.reservationId !== 'string' ||
      reserved.reservationId.length === 0 ||
      reserved.maximumBytes !== String(request.maximumBytes)
    ) {
      throw contractError('protocol.malformed', 'core', this.op('terminal-evidence.reserve'))
    }
    const reservationKey = `${request.producer}\u0000${request.scope.managerId}\u0000${request.scope.operationId}\u0000${request.scope.incarnation}`
    this.terminalReservationIds.set(reservationKey, reserved.reservationId)
    let committed = false
    let released = false
    return Object.freeze({
      maximumBytes: request.maximumBytes,
      commit: async (record: TerminalEvidenceRecord): Promise<TerminalEvidenceAppendResult> => {
        if (committed || released) {
          throw contractError('lifecycle.invalid-state', 'stream', this.op('terminal-evidence.commit'))
        }
        // Electron supplies its authenticated renderer owner before this
        // point. Direct Node callers have no renderer bridge, so bind their
        // durable record to this trusted host owner before committing it;
        // otherwise a committed row could never satisfy recovery ACK claims.
        let recoveryClaims = this.terminalEvidenceAcknowledgements.get(reserved.reservationId)
        if (recoveryClaims === undefined) {
          recoveryClaims = Object.freeze({ producer: request.producer, recipient: this.terminalEvidenceRecipient })
          await stage.call(this.central, reserved.reservationId, recoveryClaims.recipient, record.canonicalBytes)
          this.terminalEvidenceAcknowledgements.set(reserved.reservationId, recoveryClaims)
        }
        const result = await commit.call(this.central, {
          reservationId: reserved.reservationId,
          canonicalBytes: Uint8Array.from(record.canonicalBytes),
          digest: record.digest
        })
        if (result.kind === 'refused') {
          if (
            result.reason !== 'quota-exhausted' &&
            result.reason !== 'storage-unavailable' &&
            result.reason !== 'capability-unsupported'
          ) {
            throw contractError('protocol.malformed', 'core', this.op('terminal-evidence.commit'))
          }
          return Object.freeze({ kind: 'refused', reason: result.reason })
        }
        if (
          result.kind !== 'committed' ||
          typeof result.ref !== 'string' ||
          result.ref.length === 0 ||
          result.byteLength !== String(record.canonicalBytes.byteLength) ||
          result.digest !== record.digest ||
          result.formatVersion !== 1 ||
          result.durability !== 'committed'
        ) {
          throw contractError('protocol.malformed', 'core', this.op('terminal-evidence.commit'))
        }
        committed = true
        this.terminalEvidenceAcknowledgements.set(result.ref, recoveryClaims)
        return Object.freeze({
          producer: request.producer,
          ref: result.ref,
          byteLength: record.canonicalBytes.byteLength,
          digest: record.digest,
          formatVersion: 1,
          durability: 'committed',
          scope: request.scope
        })
      },
      release: async (): Promise<void> => {
        if (released) return
        await release.call(this.central, reserved.reservationId)
        this.terminalReservationIds.delete(reservationKey)
        released = true
      }
    })
  }

  /** Trusted Electron-main bridge; recipient is derived from authenticated IPC identity. */
  async stageTerminalEvidenceForRecovery(
    producer: 'scan' | 'subscription',
    scope: TerminalEvidenceReservationRequest['scope'] | null,
    recipient: string,
    canonicalBytes: Uint8Array
  ): Promise<void> {
    const stage = this.central.stageTerminalEvidence
    if (stage === undefined)
      throw contractError('capability.unsupported', 'restoration', this.op('terminal-evidence.stage'))
    if (scope === null)
      throw contractError('lifecycle.invalid-state', 'restoration', this.op('terminal-evidence.stage'))
    const key = `${producer}\u0000${scope.managerId}\u0000${scope.operationId}\u0000${scope.incarnation}`
    const reservationId = this.terminalReservationIds.get(key)
    if (reservationId === undefined)
      throw contractError('lifecycle.invalid-state', 'restoration', this.op('terminal-evidence.stage'))
    await stage.call(this.central, reservationId, recipient, canonicalBytes)
    this.terminalEvidenceAcknowledgements.set(reservationId, Object.freeze({ producer, recipient }))
  }

  /** Trusted Electron-main bridge for durable ledger restart recovery. */
  async recoverTerminalEvidenceForRecipient(
    recipient: string,
    leaseId: string
  ): Promise<
    readonly {
      readonly reservationId: string
      readonly producer: 'scan' | 'subscription'
      readonly byteLength: number
      readonly digest: string
      readonly reference: string
    }[]
  > {
    const recover = this.central.recoverTerminalEvidence
    if (recover === undefined)
      throw contractError('capability.unsupported', 'restoration', this.op('terminal-evidence.recover'))
    const commit = this.central.commitRecoveredTerminalEvidence
    if (commit === undefined)
      throw contractError('capability.unsupported', 'restoration', this.op('terminal-evidence.recover-commit'))
    try {
      const groups = await Promise.all(
        (['scan', 'subscription'] as const).map(producer =>
          recover.call(this.central, { producer, recipient, leaseId })
        )
      )
      const recovered = []
      for (const record of groups.flat()) {
        if (
          (record.state !== 'staged' && record.state !== 'committed') ||
          record.digest === undefined ||
          record.formatVersion !== 1 ||
          !/^[0-9a-f]{64}$/.test(record.digest)
        )
          throw contractError('protocol.malformed', 'restoration', this.op('terminal-evidence.recover'))
        const committed =
          record.state === 'committed'
            ? { ref: record.ref, byteLength: record.byteLength }
            : await commit.call(this.central, {
                reservationId: record.reservationId,
                leaseId,
                canonicalBytes: record.canonicalBytes,
                digest: record.digest
              })
        if (
          ('kind' in committed && committed.kind !== 'committed') ||
          committed.ref === undefined ||
          committed.byteLength === undefined ||
          !/^[1-9][0-9]*$/.test(committed.byteLength)
        )
          throw contractError('platform.failure', 'restoration', this.op('terminal-evidence.recover-commit'))
        this.terminalEvidenceAcknowledgements.set(
          committed.ref,
          Object.freeze({ producer: record.producer, recipient, reservationId: record.reservationId, leaseId })
        )
        recovered.push(
          Object.freeze({
            reservationId: record.reservationId,
            producer: record.producer,
            byteLength: Number(committed.byteLength),
            digest: record.digest,
            reference: committed.ref
          })
        )
      }
      return Object.freeze(recovered)
    } catch (error) {
      await this.releaseRecoveryLeaseAfterFailure(leaseId, error)
      throw error
    }
  }

  /**
   * Recovers evidence owned by this trusted Node host. The recipient is a
   * construction-time host identity, never application input.
   */
  recoverTerminalEvidenceForHost(leaseId: string) {
    return this.recoverTerminalEvidenceForRecipient(this.terminalEvidenceRecipient, leaseId)
  }

  /** Refresh a trusted recipient's recovery state without hiding a contended producer lane. */
  async recoverTerminalEvidenceForRecipientWithStatus(
    recipient: string,
    leaseId: string
  ): Promise<
    Readonly<{
      records: readonly {
        readonly reservationId: string
        readonly producer: 'scan' | 'subscription'
        readonly byteLength: number
        readonly digest: string
        readonly reference: string
      }[]
      busyProducers: readonly ('scan' | 'subscription')[]
    }>
  > {
    const recover = this.central.recoverTerminalEvidenceWithStatus
    if (recover === undefined)
      throw contractError('capability.unsupported', 'restoration', this.op('terminal-evidence.recover-status'))
    try {
      const groups = await Promise.all(
        (['scan', 'subscription'] as const).map(async producer => ({
          producer,
          page: await recover.call(this.central, { producer, recipient, leaseId })
        }))
      )
      const records = [] as {
        reservationId: string
        producer: 'scan' | 'subscription'
        byteLength: number
        digest: string
        reference: string
      }[]
      const busyProducers: ('scan' | 'subscription')[] = []
      for (const { producer, page } of groups) {
        if (typeof page.busy !== 'boolean')
          throw contractError('protocol.malformed', 'restoration', this.op('terminal-evidence.recover-status'))
        if (page.busy) busyProducers.push(producer)
        for (const record of page.entries) {
          if (
            (record.state !== 'staged' && record.state !== 'committed') ||
            record.digest === undefined ||
            record.formatVersion !== 1 ||
            !/^[0-9a-f]{64}$/.test(record.digest)
          )
            throw contractError('protocol.malformed', 'restoration', this.op('terminal-evidence.recover-status'))
          const commit = this.central.commitRecoveredTerminalEvidence
          if (commit === undefined)
            throw contractError(
              'capability.unsupported',
              'restoration',
              this.op('terminal-evidence.recover-status-commit')
            )
          const committed =
            record.state === 'committed'
              ? { ref: record.ref, byteLength: record.byteLength }
              : await commit.call(this.central, {
                  reservationId: record.reservationId,
                  leaseId,
                  canonicalBytes: record.canonicalBytes,
                  digest: record.digest
                })
          if (
            ('kind' in committed && committed.kind !== 'committed') ||
            committed.ref === undefined ||
            committed.byteLength === undefined ||
            !/^[1-9][0-9]*$/.test(committed.byteLength)
          )
            throw contractError('platform.failure', 'restoration', this.op('terminal-evidence.recover-status-commit'))
          this.terminalEvidenceAcknowledgements.set(
            committed.ref,
            Object.freeze({ producer: record.producer, recipient, reservationId: record.reservationId, leaseId })
          )
          records.push(
            Object.freeze({
              reservationId: record.reservationId,
              producer: record.producer,
              byteLength: Number(committed.byteLength),
              digest: record.digest,
              reference: committed.ref
            })
          )
        }
      }
      return Object.freeze({ records: Object.freeze(records), busyProducers: Object.freeze(busyProducers) })
    } catch (error) {
      await this.releaseRecoveryLeaseAfterFailure(leaseId, error)
      throw error
    }
  }

  /** Refresh recovery state for this construction-time Node host. */
  recoverTerminalEvidenceForHostWithStatus(leaseId: string) {
    return this.recoverTerminalEvidenceForRecipientWithStatus(this.terminalEvidenceRecipient, leaseId)
  }

  /** Relinquish only rows selected by this host-private recovery controller. */
  async releaseTerminalEvidenceRecoveryLease(leaseId: string): Promise<void> {
    const release = this.central.releaseTerminalEvidenceRecoveryLease
    if (release === undefined)
      throw contractError('capability.unsupported', 'restoration', this.op('terminal-evidence.recover-release'))
    await release.call(this.central, leaseId)
    for (const [reference, claims] of this.terminalEvidenceAcknowledgements) {
      if (claims.leaseId === leaseId) this.terminalEvidenceAcknowledgements.delete(reference)
    }
  }

  /** Failed selection must not strand rows or acknowledgement authority. */
  private async releaseRecoveryLeaseAfterFailure(leaseId: string, cause: unknown): Promise<void> {
    try {
      await this.releaseTerminalEvidenceRecoveryLease(leaseId)
    } catch (releaseFailure) {
      throw new AggregateError(
        [cause, releaseFailure],
        'terminal evidence recovery failed and its lease could not be released'
      )
    }
  }

  /** Internal desktop-host recovery read; public entrypoints use opaque handles. */
  readRecoveredTerminalEvidence(reference: string, maximumBytes: number) {
    return this.readTerminalEvidence(reference, maximumBytes)
  }

  /** Internal desktop-host recovery acknowledgement; claims come only from recovery. */
  acknowledgeRecoveredTerminalEvidence(reference: string): Promise<void> {
    return this.releaseTerminalEvidence(reference)
  }

  private async readTerminalEvidence(reference: string, maximumBytes: number) {
    const read = this.central.readTerminalEvidence
    if (read === undefined || !Number.isSafeInteger(maximumBytes) || maximumBytes <= 0) {
      throw contractError('argument.invalid', 'stream', this.op('terminal-evidence.read'))
    }
    const result = await read.call(this.central, reference, String(maximumBytes))
    if (result.kind === 'missing' || result.kind === 'unresolved') return Object.freeze({ kind: result.kind })
    if (result.kind !== 'committed' || result.canonicalBytes === undefined || typeof result.digest !== 'string') {
      throw contractError('protocol.malformed', 'core', this.op('terminal-evidence.read'))
    }
    const canonicalBytes = Uint8Array.from(result.canonicalBytes)
    if (canonicalBytes.byteLength > maximumBytes) {
      throw contractError('protocol.malformed', 'core', this.op('terminal-evidence.read'))
    }
    return Object.freeze({ kind: 'committed' as const, canonicalBytes, digest: result.digest })
  }

  private async releaseTerminalEvidence(reference: string): Promise<void> {
    const acknowledge = this.central.acknowledgeRecoveredTerminalEvidence
    if (acknowledge === undefined)
      throw contractError('capability.unsupported', 'stream', this.op('terminal-evidence.recovery-ack'))
    const claims = this.terminalEvidenceAcknowledgements.get(reference)
    if (claims === undefined || claims.reservationId === undefined || claims.leaseId === undefined)
      throw contractError('lifecycle.invalid-state', 'restoration', this.op('terminal-evidence.recovery-ack'))
    await acknowledge.call(this.central, {
      ref: reference,
      ...claims,
      reservationId: claims.reservationId,
      leaseId: claims.leaseId
    })
    this.terminalEvidenceAcknowledgements.delete(reference)
  }

  private terminalEvidenceFromCore(
    value: unknown,
    producer: 'scan' | 'subscription',
    scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string } | null,
    operation: string
  ): TerminalEvidenceCommit | undefined {
    if (value == null) return undefined
    if (typeof value !== 'object' || scope === null) {
      throw contractError('protocol.malformed', 'core', `${operation}.terminal-evidence`)
    }
    const candidate = value
    const sourceScope = Reflect.get(candidate, 'scope')
    const ref = Reflect.get(candidate, 'ref')
    const byteLength = Reflect.get(candidate, 'byteLength')
    const digest = Reflect.get(candidate, 'digest')
    if (
      Reflect.get(candidate, 'producer') !== producer ||
      typeof ref !== 'string' ||
      ref.length === 0 ||
      !/^[0-9]+$/.test(String(byteLength)) ||
      !Number.isSafeInteger(Number(byteLength)) ||
      typeof digest !== 'string' ||
      Reflect.get(candidate, 'formatVersion') !== 1 ||
      Reflect.get(candidate, 'durability') !== 'committed' ||
      typeof sourceScope !== 'object' ||
      sourceScope === null ||
      Reflect.get(sourceScope, 'managerId') !== scope.managerId ||
      Reflect.get(sourceScope, 'operationId') !== scope.operationId ||
      Reflect.get(sourceScope, 'incarnation') !== scope.incarnation
    ) {
      throw contractError('protocol.malformed', 'core', `${operation}.terminal-evidence`)
    }
    return Object.freeze({
      producer,
      ref,
      byteLength: Number(byteLength),
      digest,
      formatVersion: 1,
      durability: 'committed',
      scope
    })
  }

  private async commitPendingTerminalEvidence(
    pending: { readonly reservationId: string; readonly pendingToken: string } | null | undefined,
    producer: 'scan' | 'subscription',
    scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string } | null,
    terminal: import('../../backend-contract/streams').StreamTerminalNotice,
    operation: string
  ): Promise<TerminalEvidenceCommit | undefined> {
    if (pending == null) return undefined
    if (scope === null || typeof pending.reservationId !== 'string' || typeof pending.pendingToken !== 'string')
      throw contractError('protocol.malformed', 'core', `${operation}.terminal-evidence-pending`)
    const key = `${producer}\u0000${scope.managerId}\u0000${scope.operationId}\u0000${scope.incarnation}`
    if (this.terminalReservationIds.get(key) !== pending.reservationId)
      throw contractError('protocol.violation', 'stream', `${operation}.terminal-evidence-reservation`)
    const commit = this.central.commitTerminalEvidence
    const acknowledge = this.central.acknowledgeTerminalEvidence
    if (commit === undefined || acknowledge === undefined)
      throw contractError('capability.unsupported', 'stream', `${operation}.terminal-evidence-ack`)
    const canonicalBytes = encodeTerminalEvidence(terminal, scope)
    const digest = createHash('sha256').update(canonicalBytes).digest('hex')
    const result = await commit.call(this.central, { reservationId: pending.reservationId, canonicalBytes, digest })
    if (result.kind === 'refused') return undefined
    if (
      result.kind !== 'committed' ||
      result.ref === undefined ||
      result.byteLength !== String(canonicalBytes.byteLength) ||
      result.digest !== digest ||
      result.formatVersion !== 1 ||
      result.durability !== 'committed'
    )
      throw contractError('protocol.malformed', 'core', `${operation}.terminal-evidence-commit`)
    await acknowledge.call(this.central, { pendingToken: pending.pendingToken })
    this.terminalReservationIds.delete(key)
    return Object.freeze({
      producer,
      ref: result.ref,
      byteLength: canonicalBytes.byteLength,
      digest,
      formatVersion: 1,
      durability: 'committed',
      scope
    })
  }

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    const operation = this.op('scan.start')
    this.assertOperational(operation)
    const nativeFilter = this.scanFilterFor(options, operation)
    if (this.scanGroup !== null) {
      // Finding 209: the refusal names the occupying scan (session, owner
      // lease, state) so a scan left open — for example by an advertisement
      // capture that returned without stopping — is diagnosable.
      const active = this.scanGroup
      throw contractError('scan.already-active', 'scan', operation, {
        domain: 'desktop-rust-core',
        code: 'scan-arbitration',
        safeMessage: `scan ${String(active.scanSessionId)} is still active; stop it before starting a new scan`,
        metadata: Object.freeze({
          scanSessionId: String(active.scanSessionId),
          ownerLeaseId: String(active.ownerLeaseId),
          state: active.state
        })
      })
    }
    const ordinal = this.nextOrdinal()
    const correlation = String(this.mintedCorrelation())
    const started = await this.withTicket(correlation, options.signal, operation, ticket =>
      this.central.startScan({
        owner: `${this.owner}/scan-${ordinal}`,
        serviceUuids: nativeFilter.serviceUuids.map(uuid => String(uuid)),
        duplicatePolicy: options.duplicatePolicy,
        ...(options.terminalEvidenceScope === undefined
          ? {}
          : { terminalEvidenceScope: options.terminalEvidenceScope }),
        ...(options.platform?.kind === 'winrt'
          ? {
              winrtScanningMode: options.platform.mode,
              winrtAllowExtendedAdvertisements: options.platform.allowExtendedAdvertisements
            }
          : {}),
        ...(nativeFilter.localNamePrefix === null || nativeFilter.localNamePrefix.length === 0
          ? {}
          : { localNamePrefix: nativeFilter.localNamePrefix }),
        ticket,
        ...this.budget(options)
      })
    )
    if (typeof started.operationId !== 'string' || started.operationId.length === 0) {
      throw contractError('protocol.malformed', 'core', `${operation}.shape`)
    }
    const scanOrdinal = this.mintScanOrdinal()
    const platform = this.profile.platform
    const ownerLeaseId = this.identifiers.leaseId(LEGACY_DESKTOP_NAMES[platform].scanLease(scanOrdinal))
    const group: ScanGroup = {
      coreOperationId: started.operationId,
      scanSessionId: this.identifiers.scanSessionId(`${platform}-scan-session-${scanOrdinal}`),
      ownerLeaseId,
      shareToken: options.sharing.allowSharing
        ? this.identifiers.scanShareToken(`${platform}-scan-share-${scanOrdinal}`)
        : null,
      consumers: new Map(),
      state: 'active',
      stopResult: null,
      foreignRefused: 0,
      evidence: new ScanEvidenceSession()
    }
    this.scanGroup = group
    const consumer = this.addScanConsumer(group, ownerLeaseId, options, options.filter)
    this.ensureScanPump()
    return this.scanLease(group, consumer)
  }

  private async joinScan(
    sharedLeaseId: LeaseId<string, string>,
    shareToken: ScanShareToken<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    const operation = this.op('scan.join')
    this.assertOperational(operation)
    const group = this.scanGroup
    if (
      group === null ||
      group.state !== 'active' ||
      group.ownerLeaseId !== sharedLeaseId ||
      group.shareToken === null ||
      group.shareToken !== shareToken
    ) {
      throw contractError('ownership.denied', 'scan', operation)
    }
    const owner = group.consumers.get(String(group.ownerLeaseId))
    if (owner === undefined) {
      throw contractError('lifecycle.invariant-violation', 'scan', `${operation}.owner`)
    }
    const leaseId = this.identifiers.leaseId(
      LEGACY_DESKTOP_NAMES[this.profile.platform].scanLease(this.mintScanOrdinal())
    )
    const consumer = this.addScanConsumer(group, leaseId, owner.options, owner.filter)
    return this.scanLease(group, consumer)
  }

  private addScanConsumer(
    group: ScanGroup,
    leaseId: LeaseId<string, string>,
    options: OwnerScanOptions<string, string>,
    filter: ScanFilter
  ): ScanConsumer {
    const consumer: ScanConsumer = {
      leaseId,
      options,
      filter,
      stream: new CoreBoundedStream<AdvertisementObservation<string>>(
        options.delivery,
        options.delivery.overflowPolicy
      ),
      seenPeers: new Set(),
      malformedDropped: 0,
      removeAbort: () => undefined
    }
    group.consumers.set(String(leaseId), consumer)
    const signal = options.signal
    if (signal !== null && leaseId === group.ownerLeaseId) {
      const onAbort = (): void => {
        this.stopScanConsumer(group, consumer)
          .then(cleanup => {
            if (cleanup.state !== 'released')
              this.noteDiagnostic('scan-abort-stop-failed', 'scan stop after abort did not release')
          })
          .catch(error => this.noteBackgroundFailure('scan-abort-stop-failed', error))
      }
      signal.addEventListener('abort', onAbort, { once: true })
      consumer.removeAbort = () => signal.removeEventListener('abort', onAbort)
    }
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

  /**
   * A joined consumer leaves on its own; the owner's stop ends the one core
   * scan and every consumer with it (the legacy share semantics). The scan
   * stays owned until the core confirms the OS stop.
   */
  private async stopScanConsumer(group: ScanGroup, consumer: ScanConsumer): Promise<CleanupRecord> {
    consumer.removeAbort()
    if (consumer.leaseId !== group.ownerLeaseId) {
      group.consumers.delete(String(consumer.leaseId))
      if (!consumer.stream.isTerminal()) consumer.stream.finishWithReason('owner-released')
      return Object.freeze({ state: 'released', failures: Object.freeze([]) })
    }
    if (group.stopResult === null) {
      group.state = 'stopping'
      group.stopResult = this.stopCoreScan(group)
    }
    return group.stopResult
  }

  private async stopCoreScan(group: ScanGroup): Promise<CleanupRecord> {
    const operation = this.op('scan.stop')
    try {
      await this.central.stopScan(group.coreOperationId)
    } catch (error) {
      // The core keeps the scan owned after a failed stop; a later stop
      // retries the OS call.
      group.state = 'active'
      group.stopResult = null
      const normalized = desktopRustCoreError(error, operation).normalized
      return Object.freeze({
        state: 'release-failed',
        failures: Object.freeze([{ resourceKind: 'scan', error: normalized }])
      })
    }
    group.state = 'stopped'
    for (const current of group.consumers.values()) {
      current.removeAbort()
      if (!current.stream.isTerminal()) current.stream.finishWithReason('owner-released')
    }
    group.consumers.clear()
    if (this.scanGroup === group) this.scanGroup = null
    return Object.freeze({ state: 'released', failures: Object.freeze([]) })
  }

  /** Start the one provider-owned reader of the core's process-wide scan queue. */
  private ensureScanPump(): void {
    if (this.destroyed || this.scanPumpRunning !== null || this.scanGroup === null) return
    const pump = this.pumpScanObservations()
    this.scanPumpRunning = pump
    pump
      .catch(error => {
        this.noteBackgroundFailure('scan-pump-failed', error)
        this.noteDiagnostic('scan-pump-failed', 'the provider scan pump failed outside a scan take')
      })
      .finally(() => {
        this.scanPumpRunning = null
        // A stop can make the prior turn retire while a new scan starts. The
        // successor must still get the sole reader rather than waiting for a
        // future start that may never happen.
        this.ensureScanPump()
      })
  }

  private noteForeignScanObservation(group: ScanGroup, scanOperationId: string): void {
    group.foreignRefused += 1
    this.noteDiagnostic('scan-observation-foreign', 'an observation queued for another scan was refused', {
      scanOperationId: String(scanOperationId).slice(0, 128),
      refused: group.foreignRefused
    })
  }

  private async pumpScanObservations(): Promise<void> {
    while (!this.destroyed) {
      const groupAtTake = this.scanGroup
      if (groupAtTake === null) return
      const polledAt = this.wakeSequence
      let taken: DesktopRustCoreScanObservation | null
      try {
        taken = await this.central.takeScanObservation()
      } catch (error) {
        await this.failScanSource(groupAtTake, 'scan-pump-failed', error)
        continue
      }
      if (taken === null || taken === undefined) {
        await this.pumpWait(polledAt)
        continue
      }
      const group = this.scanGroup
      if (group === null || group.state !== 'active') continue
      // 4.x reported advertisements only while a scan was active: a
      // sighting the core queued for another scan is never attributed to
      // this one (LEGACY-AUDIT-5 S2). It is refused loudly, not delivered.
      if (taken.scanOperationId !== group.coreOperationId) {
        this.noteForeignScanObservation(group, taken.scanOperationId)
        continue
      }
      const next = taken.advertisement
      try {
        await this.ensureAddressType(next)
      } catch (error) {
        await this.failScanSource(group, 'scan-pump-failed', error)
        continue
      }
      let observation: AdvertisementObservation<string>
      try {
        observation = this.mapObservation(next, group.scanSessionId, taken.ageMs)
      } catch (error) {
        this.noteBackgroundFailure('scan-record-malformed', error)
        this.countMalformedObservation(group)
        continue
      }
      // Address resolution may await. Never emit an old scan's observation
      // into a group that stopped or was replaced while that work ran.
      const current = this.scanGroup
      if (current === null || current.state !== 'active') continue
      if (taken.scanOperationId !== current.coreOperationId) {
        this.noteForeignScanObservation(current, taken.scanOperationId)
        continue
      }
      this.deliverObservation(current, observation, next.peerId)
    }
  }

  /**
   * The scan source stopped (a failed take, or the pump itself failed):
   * every consumer ends `source-failed` and the owner's scan is stopped, so
   * no stream stays open on a pump that will never deliver again.
   */
  private async failScanSource(group: ScanGroup, code: string, error: unknown): Promise<void> {
    if (this.destroyed) return
    this.noteBackgroundFailure(code, error)
    if (this.scanGroup !== group) {
      this.noteDiagnostic(
        code,
        this.scanGroup === null
          ? 'the provider scan pump failed after its scan stopped'
          : 'the provider scan pump failed after ownership moved to another scan'
      )
      return
    }
    for (const consumer of group.consumers.values()) consumer.stream.closeWithReason('source-failed')
    const owner = group.consumers.get(String(group.ownerLeaseId))
    if (owner === undefined) return
    const cleanup = await this.stopScanConsumer(group, owner)
    if (cleanup.state !== 'released')
      this.noteDiagnostic('scan-stop-failed', 'scan stop after source failure did not release')
  }

  /**
   * An observation the core handed over malformed is lost before routing:
   * every consumer that could have received it counts it in its drop
   * accounting, and the scan continues.
   */
  private countMalformedObservation(group: ScanGroup): void {
    for (const consumer of group.consumers.values()) {
      consumer.malformedDropped += 1
      consumer.stream.observeSourceOverflow({
        kind: 'overflow',
        policy: 'drop-newest',
        droppedItems: resourceCount(consumer.malformedDropped),
        droppedBytes: resourceCount(0),
        replacedItems: resourceCount(0)
      })
    }
  }

  private deliverObservation(
    group: ScanGroup,
    observation: AdvertisementObservation<string>,
    nativePeerId: string
  ): void {
    for (const consumer of [...group.consumers.values()]) {
      if (consumer.stream.isTerminal()) continue
      const matched = group.evidence.matchAdvertisement(observation, candidate =>
        advertisementMatchesFilter(consumer.filter, candidate)
      )
      if (matched === null) continue
      if (consumer.options.duplicatePolicy === 'first') {
        if (consumer.seenPeers.has(nativePeerId)) continue
        consumer.seenPeers.add(nativePeerId)
      }
      const push = consumer.stream.emit(matched, ADVERTISEMENT_BYTES)
      if (push.terminated && consumer.leaseId !== group.ownerLeaseId) {
        group.consumers.delete(String(consumer.leaseId))
      }
    }
  }

  /**
   * The LE address type of a newly seen addressed peer, asked once and
   * cached. A radio that does not report types answers `unsupported`: the
   * address then stays `opaque`, never a guessed public/random.
   */
  private async ensureAddressType(advertisement: DesktopRustCoreAdvertisement): Promise<void> {
    if (typeof advertisement.address !== 'string' || advertisement.address.length === 0) return
    if (advertisement.addressType !== undefined) {
      this.addressTypes.set(advertisement.peerId, advertisement.addressType)
      return
    }
    if (this.addressTypes.has(advertisement.peerId)) return
    this.peerIdForNativeId(advertisement.peerId)
    try {
      this.addressTypes.set(advertisement.peerId, await this.central.addressType({ peerId: advertisement.peerId }))
    } catch (error) {
      const normalized = desktopRustCoreError(error, this.op('scan.address-type')).normalized
      this.addressTypes.set(advertisement.peerId, null)
      if (normalized.code !== 'capability.unsupported') this.noteBackgroundFailure('address-type-failed', error)
    }
  }

  /**
   * The observation's label (LEGACY-AUDIT-5 S2), as the 4.x backend of each
   * OS labelled it: the OS's merged device state is derived data; one
   * advertisement's own data is raw on WinRT and derived on CoreBluetooth
   * (its parsed advertisement dictionary) and BlueZ.
   */
  private observationProvenance(source: unknown, operation: string): 'platform-raw' | 'platform-derived' {
    if (source === 'device-state') return 'platform-derived'
    if (source === 'advertisement') return this.profile.platform === 'winrt' ? 'platform-raw' : 'platform-derived'
    throw contractError('protocol.malformed', 'core', `${operation}.source`)
  }

  private mapObservation(
    value: DesktopRustCoreAdvertisement,
    scanSessionId: ScanSessionId<string, string>,
    ageMs: number
  ): AdvertisementObservation<string> {
    const operation = this.op('scan.observation')
    if (typeof value.peerId !== 'string' || value.peerId.length === 0) {
      throw contractError('protocol.malformed', 'core', `${operation}.peer`)
    }
    if (typeof ageMs !== 'number' || !Number.isFinite(ageMs) || ageMs < 0) {
      throw contractError('protocol.malformed', 'core', `${operation}.age`)
    }
    const provenance = this.observationProvenance(value.source, operation)
    // Stamped when the core received it, on this host's clock.
    const receivedAt = Math.max(0, this.now() - ageMs)
    const peerId = this.peerIdForNativeId(value.peerId)
    // F8: unprovided fields report per-host vocabulary (CoreBluetooth
    // `unavailable`, elsewhere `absent`).
    const unprovided = <Item>(reason: string): AdvertisementField<Item> =>
      unprovidedField<Item>(this.profile.platform, reason)
    const present = <Item>(found: Item | null, reason: string): AdvertisementField<Item> =>
      found === null ? unprovided<Item>(reason) : presentField(found)
    const reported = <Item>(found: Item | null | undefined, reason: string): AdvertisementField<Item> =>
      found === null || found === undefined ? unprovided<Item>(reason) : presentField(found)
    return Object.freeze({
      device: Object.freeze({
        id: peerId,
        backendInstanceId: this.attachment.backendInstanceId,
        // F8: backend-scoped, as every legacy backend reported it.
        scope: 'backend',
        stableAcrossRestarts: false,
        address:
          typeof value.address === 'string' && value.address.length > 0
            ? Object.freeze({
                value: value.address,
                type:
                  value.addressType === undefined
                    ? (this.addressTypes.get(value.peerId) ?? 'opaque')
                    : (value.addressType ?? 'opaque')
              })
            : null
      }),
      provenance,
      ...(value.source === undefined ? {} : { origin: value.source }),
      sourceTimestamp: unprovided<SourceTimestamp>('source timestamp not reported by the dispatch surface'),
      receivedAtMonotonicMs: monotonicTimestamp(receivedAt),
      ingressOrdinal: this.nextEventOrdinal(),
      scanSessionId,
      localName: present<string>(
        typeof value.localName === 'string' ? value.localName : null,
        'local name not reported by this radio'
      ),
      rssi: present<number>(typeof value.rssi === 'number' ? value.rssi : null, 'RSSI not reported by this radio'),
      txPower: present<number>(
        typeof value.txPower === 'number' ? value.txPower : null,
        'TX power not reported by this radio'
      ),
      connectable: reported<boolean>(value.connectable, 'connectable not reported by this radio'),
      appearance: unprovided<number>('appearance not reported by the dispatch surface'),
      // B-R4: BlueZ cannot distinguish "none" from "not reported": no
      // UUIDs property is absent, as its legacy backend mapped it — never
      // present([]). Other radios report their own answer.
      serviceUuids:
        this.profile.platform === 'bluez' && value.serviceUuids.length === 0
          ? absentField<readonly Uuid[]>('no UUIDs reported by BlueZ ObjectManager')
          : presentField<readonly Uuid[]>(
              Object.freeze(value.serviceUuids.map(entry => uuidFromCore(entry, `${operation}.service-uuids`)))
            ),
      solicitedServiceUuids: reported<readonly Uuid[]>(
        uuidListFromCore(value.solicitedServiceUuids, `${operation}.solicited`),
        'solicited services not reported by this radio'
      ),
      overflowServiceUuids: reported<readonly Uuid[]>(
        uuidListFromCore(value.overflowServiceUuids, `${operation}.overflow`),
        'overflow services not reported by this radio'
      ),
      serviceData: presentField(
        Object.freeze(
          value.serviceData.map(entry =>
            Object.freeze({
              serviceUuid: uuidFromCore(entry.uuid, `${operation}.service-data`),
              value: ownedBytes(bytesFromCore(entry.payload, operation))
            })
          )
        )
      ),
      manufacturerData: presentField(
        Object.freeze(
          value.manufacturerData.map(entry =>
            Object.freeze({
              companyIdentifier: Number(entry.companyId),
              value: ownedBytes(bytesFromCore(entry.payload, operation))
            })
          )
        )
      ),
      rawRecord: unprovided<OwnedBytes>('raw record not reported by the dispatch surface'),
      scanResponseRecord: unprovided<OwnedBytes>('scan response record not reported by the dispatch surface')
    })
  }

  // -- connection parameters ---------------------------------------------------

  private readConnectionParameters<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ConnectionParametersRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionParametersMeasurement<string, Operation>> {
    const operation = this.op('connection.parameters')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<ConnectionParametersMeasurement<string, Operation>> => {
      const measured = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.connectionParameters({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(request.operation)
        })
      )
      return Object.freeze({
        connectionId: connection.connectionId,
        connectionGeneration: connection.connectionGeneration,
        intervalUs: measured.intervalUs,
        latency: measured.latency,
        supervisionTimeoutUs: measured.supervisionTimeoutUs,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  private async watchConnectionParameters(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions = { signal: null, deadline: null }
  ): Promise<ConnectionParametersWatch<string>> {
    const operation = this.op('connection.parameters')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    if (
      record.parameterSourceFailure?.code === 'stream.closed' ||
      record.parameterSourceFailure?.code === 'stream.quota'
    )
      throw new BackendContractError(record.parameterSourceFailure)
    const sourceRevision = record.parameterSourceRevision ?? 0
    const correlation = String(this.mintedCorrelation())
    const stream = new CoreBoundedStream<ConnectionParametersStreamObservation<string>>(
      { itemCapacity: capacity(64), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const openingCancellation = new AbortController()
    const onAbort = (): void => openingCancellation.abort()
    if (options.signal?.aborted === true) openingCancellation.abort()
    else options.signal?.addEventListener('abort', onAbort, { once: true })
    const watch: ParameterWatch = {
      record,
      stream,
      ordinal: 0,
      acceptanceRevision: 0,
      probed: false,
      failure: null,
      buffered: [],
      openingCancellation
    }
    this.parameterWatches.add(watch)
    const generation = record.coreGeneration
    const revision = watch.acceptanceRevision
    const probeStartedAt = this.now()
    let measured: { intervalUs: number; latency: number; supervisionTimeoutUs: number } | null = null
    try {
      measured = await this.withTicket(correlation, openingCancellation.signal, operation, ticket =>
        this.central.connectionParameters({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          observationWatch: true,
          ...this.budget(options)
        })
      )
      await this.acceptParameterReportsDuringProbe(watch)
      if (watch.acceptanceRevision === revision)
        assertConnectionParameterValues(measured, this.op('connection.parameters.result'))
    } catch (error) {
      await this.acceptParameterReportsDuringProbe(watch)
      if (watch.failure !== null) throw new BackendContractError(watch.failure)
      if (!this.parameterWatches.has(watch))
        throw contractError('connection.stale', 'connection', `${operation}.closed`)
      if (record.coreGeneration !== generation || record.state !== 'connected') {
        this.closeParameterWatch(watch, 'connection-lost')
        throw contractError('connection.stale', 'connection', `${operation}.closed`)
      }
      if (
        watch.acceptanceRevision === revision ||
        desktopRustCoreError(error, operation).normalized.code === 'operation.aborted' ||
        desktopRustCoreError(error, operation).normalized.code === 'operation.timed-out' ||
        openingCancellation.signal.aborted ||
        record.coreGeneration !== generation ||
        record.state !== 'connected' ||
        (options.deadline != null && this.now() >= options.deadline)
      ) {
        this.closeParameterWatch(watch, 'source-failed', desktopRustCoreError(error, operation).normalized)
        throw error
      }
    } finally {
      options.signal?.removeEventListener('abort', onAbort)
    }
    if (watch.failure !== null) throw new BackendContractError(watch.failure)
    try {
      this.assertPeerQueryUsable(operation, options)
    } catch (error) {
      this.closeParameterWatch(watch, 'source-failed', desktopRustCoreError(error, operation).normalized)
      throw error
    }
    if (!this.parameterWatches.has(watch) || record.coreGeneration !== generation || record.state !== 'connected') {
      if (record.coreGeneration !== generation || record.state !== 'connected')
        this.closeParameterWatch(watch, 'connection-lost')
      if (watch.failure !== null) throw new BackendContractError(watch.failure)
      throw contractError('connection.stale', 'connection', `${operation}.closed`)
    }
    if ((record.parameterSourceRevision ?? 0) === sourceRevision) record.parameterSourceFailure = null
    watch.probed = true
    // Native events accepted while the getter was pending have an established
    // source order. The delayed getter has no comparable capture ordinal.
    if (watch.acceptanceRevision === revision && measured !== null)
      this.emitConnectionParameters(watch, measured, probeStartedAt)
    for (const report of watch.buffered.splice(0))
      this.emitConnectionParameters(watch, report, report.observedAtMonotonicMs, false)
    return Object.freeze({
      events: stream,
      close: async (): Promise<CleanupRecord> => {
        this.closeParameterWatch(watch, 'owner-released')
        return Object.freeze({ state: 'released', failures: Object.freeze([]) })
      }
    })
  }

  private closeParameterWatch(
    watch: ParameterWatch,
    reason: 'owner-released' | 'connection-lost' | 'source-failed',
    normalized: NormalizedBleError | null = null
  ): void {
    this.parameterWatches.delete(watch)
    if (normalized !== null && watch.failure === null) watch.failure = normalized
    if (!watch.probed) watch.openingCancellation.abort()
    watch.buffered.length = 0
    watch.stream.closeWithReason(reason, normalized)
  }

  private emitConnectionParameters(
    watch: ParameterWatch,
    measured: { readonly intervalUs: number; readonly latency: number; readonly supervisionTimeoutUs: number },
    observedAtMonotonicMs = this.now(),
    accept = true
  ): void {
    if (!this.parameterWatches.has(watch)) return
    try {
      assertConnectionParameterValues(measured, this.op('connection.parameters.result'))
    } catch (error) {
      this.closeParameterWatch(
        watch,
        'source-failed',
        desktopRustCoreError(error, this.op('connection.parameters.result')).normalized
      )
      return
    }
    if (accept) watch.acceptanceRevision += 1
    if (!watch.probed) {
      if (watch.buffered.length >= 64) {
        this.closeParameterWatch(
          watch,
          'source-failed',
          contractError('stream.overflow', 'stream', this.op('connection.parameters.initial-buffer')).normalized
        )
      } else {
        watch.buffered.push({ ...measured, observedAtMonotonicMs })
      }
      return
    }
    watch.ordinal += 1
    const observation: ConnectionParametersStreamObservation<string> = Object.freeze({
      connectionId: watch.record.path.connectionId,
      connectionGeneration: watch.record.path.connectionGeneration,
      intervalUs: measured.intervalUs,
      latency: measured.latency,
      supervisionTimeoutUs: measured.supervisionTimeoutUs,
      observedAtMonotonicMs,
      ordinal: watch.ordinal
    })
    if (watch.stream.emit(observation, 128).terminated) this.parameterWatches.delete(watch)
  }

  private applyConnectionParameters(event: DesktopRustCoreConnectionParametersEvent): void {
    if (event.kind === 'closed') {
      this.failCoreEventSource('connection-parameter-events-closed')
      return
    }
    if (event.kind !== 'state' && event.kind !== 'source-failed') return
    let sourceFailure: NormalizedBleError | null = null
    if (event.kind === 'source-failed') {
      sourceFailure =
        typeof event.error === 'string'
          ? desktopRustCoreError(event.error, this.op('connection.parameters.event')).normalized
          : contractError('protocol.violation', 'connection', this.op('connection.parameters.event-error')).normalized
    } else {
      try {
        assertConnectionParameterValues(event, this.op('connection.parameters.event'))
      } catch (error) {
        sourceFailure = desktopRustCoreError(error, this.op('connection.parameters.event')).normalized
      }
    }
    for (const record of this.connectionsById.values()) {
      if (
        record.state === 'connected' &&
        record.nativePeerId === event.peerId &&
        record.coreGeneration === event.connectionGeneration
      ) {
        record.parameterSourceRevision = (record.parameterSourceRevision ?? 0) + 1
        record.parameterSourceFailure = sourceFailure
      }
    }
    for (const watch of [...this.parameterWatches]) {
      if (watch.record.nativePeerId !== event.peerId) continue
      if (event.connectionGeneration !== watch.record.coreGeneration) continue
      if (watch.record.state !== 'connected') {
        this.closeParameterWatch(watch, 'connection-lost')
        continue
      }
      if (sourceFailure !== null) {
        this.closeParameterWatch(watch, 'source-failed', sourceFailure)
        continue
      }
      if (
        typeof event.intervalUs !== 'number' ||
        typeof event.latency !== 'number' ||
        typeof event.supervisionTimeoutUs !== 'number'
      ) {
        this.closeParameterWatch(
          watch,
          'source-failed',
          contractError('protocol.violation', 'connection', this.op('connection.parameters.event')).normalized
        )
        continue
      }
      this.emitConnectionParameters(watch, {
        intervalUs: event.intervalUs,
        latency: event.latency,
        supervisionTimeoutUs: event.supervisionTimeoutUs
      })
    }
  }

  private async acceptParameterReportsDuringProbe(watch: ParameterWatch): Promise<void> {
    if (typeof this.central.takeConnectionParameterEvent !== 'function') return
    try {
      for (let count = 0; count <= 4096; count += 1) {
        const event = await this.central.takeConnectionParameterEvent()
        if (event == null) return
        if (event.kind === 'lagged') {
          this.closeParameterWatch(
            watch,
            'source-failed',
            contractError('stream.overflow', 'stream', this.op('connection.parameters.concurrent-gap')).normalized
          )
          return
        }
        this.applyConnectionParameters(event)
      }
      this.closeParameterWatch(
        watch,
        'source-failed',
        contractError('stream.quota', 'stream', this.op('connection.parameters.probe-fence')).normalized
      )
    } catch (error) {
      this.closeParameterWatch(
        watch,
        'source-failed',
        desktopRustCoreError(error, this.op('connection.parameters.probe-fence')).normalized
      )
    }
  }

  private async reconcileConnectionParameters(missed: number | null): Promise<void> {
    this.noteDiagnostic(
      'connection-parameter-events-lagged',
      'connection-parameter reports were missed; re-reading them',
      { missed }
    )
    for (const watch of [...this.parameterWatches]) {
      if (watch.record.state !== 'connected') {
        this.closeParameterWatch(watch, 'connection-lost')
        continue
      }
      const revision = watch.acceptanceRevision
      const generation = watch.record.coreGeneration
      try {
        const measured = await this.central.connectionParameters({
          peerId: watch.record.nativePeerId,
          lease: watch.record.lease,
          observationWatch: true
        })
        await this.acceptParameterReportsDuringProbe(watch)
        if (
          this.parameterWatches.has(watch) &&
          watch.acceptanceRevision === revision &&
          watch.record.coreGeneration === generation &&
          watch.record.state === 'connected'
        )
          this.emitConnectionParameters(watch, measured)
      } catch (error) {
        await this.acceptParameterReportsDuringProbe(watch)
        if (
          this.parameterWatches.has(watch) &&
          watch.acceptanceRevision === revision &&
          watch.record.coreGeneration === generation &&
          watch.record.state === 'connected'
        )
          this.closeParameterWatch(
            watch,
            'source-failed',
            desktopRustCoreError(error, this.op('connection.parameters.reconcile')).normalized
          )
      }
    }
  }

  // -- write-without-response readiness ---------------------------------------

  /**
   * A readiness watch for one connection: the core's probe answers the
   * current state, then each OS readiness report for this connection's
   * generation follows (legacy CoreBluetooth `onWriteWithoutResponseReadiness`).
   * Reports arriving before the probe resolves buffer (latest wins) and
   * replay after it (F3); while the link stays unready the probe re-runs
   * every 100 ms as a safety net against a lost OS edge (F2).
   */
  private async writeWithoutResponseReadiness(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions = { signal: null, deadline: null }
  ): Promise<ConnectionWriteReadinessWatch<string>> {
    const operation = this.op('gatt.write-readiness')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(this.mintedCorrelation())
    const stream = new CoreBoundedStream<ConnectionWriteReadinessObservation<string>>(
      { itemCapacity: capacity(64), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
      // F1: the legacy watch dropped the oldest observation on overflow.
      'drop-oldest'
    )
    const openingCancellation = new AbortController()
    const onAbort = (): void => openingCancellation.abort()
    if (options.signal?.aborted === true) openingCancellation.abort()
    else options.signal?.addEventListener('abort', onAbort, { once: true })
    const watch: ReadinessWatch = {
      record,
      stream,
      ordinal: 0,
      acceptanceRevision: 0,
      failure: null,
      openingCancellation,
      ready: false,
      probed: false,
      buffered: null,
      reprobeTimer: null,
      deadline: options.deadline ?? null,
      signal: options.signal
    }
    // F3: registered before the probe so reports arriving mid-probe buffer
    // instead of dropping.
    this.readinessWatches.add(watch)
    const generation = record.coreGeneration
    const revision = watch.acceptanceRevision
    let ready: boolean | null = null
    try {
      ready = await this.withTicket(correlation, openingCancellation.signal, operation, ticket =>
        this.central.writeReadiness({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(options)
        })
      )
      await this.acceptReadinessReportsDuringProbe(watch)
    } catch (error) {
      await this.acceptReadinessReportsDuringProbe(watch)
      if (watch.failure !== null) throw new BackendContractError(watch.failure)
      if (!this.readinessWatches.has(watch))
        throw contractError('connection.stale', 'connection', `${operation}.closed`)
      if (record.coreGeneration !== generation || record.state !== 'connected') {
        this.closeReadinessWatch(watch, 'connection-lost')
        throw contractError('connection.stale', 'connection', `${operation}.closed`)
      }
      if (
        watch.acceptanceRevision === revision ||
        desktopRustCoreError(error, operation).normalized.code === 'operation.aborted' ||
        desktopRustCoreError(error, operation).normalized.code === 'operation.timed-out' ||
        openingCancellation.signal.aborted ||
        record.coreGeneration !== generation ||
        record.state !== 'connected' ||
        (options.deadline != null && this.now() >= options.deadline)
      ) {
        this.closeReadinessWatch(watch, 'source-failed', desktopRustCoreError(error, operation).normalized)
        throw error
      }
    } finally {
      options.signal?.removeEventListener('abort', onAbort)
    }
    if (watch.failure !== null) throw new BackendContractError(watch.failure)
    try {
      this.assertPeerQueryUsable(operation, options)
    } catch (error) {
      this.closeReadinessWatch(watch, 'source-failed', desktopRustCoreError(error, operation).normalized)
      throw error
    }
    if (!this.readinessWatches.has(watch) || record.coreGeneration !== generation || record.state !== 'connected') {
      if (record.coreGeneration !== generation || record.state !== 'connected')
        this.closeReadinessWatch(watch, 'connection-lost')
      if (watch.failure !== null) throw new BackendContractError(watch.failure)
      // The watch ended while the probe was in flight (connection lost).
      throw contractError('connection.stale', 'connection', `${operation}.closed`)
    }
    watch.probed = true
    if (watch.acceptanceRevision === revision && ready !== null) this.emitReadiness(watch, ready)
    const pending = watch.buffered
    watch.buffered = null
    // F3: replay the latest pre-probe report for this generation, if any.
    if (
      pending !== null &&
      typeof pending.generation === 'string' &&
      pending.generation === watch.record.coreGeneration
    ) {
      this.emitReadiness(watch, pending.ready, false)
    }
    if (!watch.ready) this.scheduleReadinessReprobe(watch)
    return Object.freeze({
      events: stream,
      close: async (): Promise<CleanupRecord> => {
        this.closeReadinessWatch(watch, 'owner-released')
        return Object.freeze({ state: 'released', failures: Object.freeze([]) })
      }
    })
  }

  /** Close a readiness watch: stop its reprobe, end its stream, forget it. */
  private closeReadinessWatch(
    watch: ReadinessWatch,
    reason: 'owner-released' | 'connection-lost' | 'source-failed',
    normalized: NormalizedBleError | null = null
  ): void {
    if (watch.reprobeTimer !== null) {
      clearTimeout(watch.reprobeTimer)
      watch.reprobeTimer = null
    }
    this.readinessWatches.delete(watch)
    if (normalized !== null && watch.failure === null) watch.failure = normalized
    if (!watch.probed) watch.openingCancellation.abort()
    watch.buffered = null
    watch.stream.closeWithReason(reason, normalized)
  }

  private emitReadiness(watch: ReadinessWatch, ready: boolean, accept = true): void {
    if (!this.readinessWatches.has(watch)) return
    if (accept) watch.acceptanceRevision += 1
    if (!watch.probed) {
      watch.buffered = Object.freeze({ ready, generation: watch.record.coreGeneration })
      return
    }
    watch.ready = ready
    watch.ordinal += 1
    const observation: ConnectionWriteReadinessObservation<string> = Object.freeze({
      connectionId: watch.record.path.connectionId,
      connectionGeneration: watch.record.path.connectionGeneration,
      ready,
      observedAtMonotonicMs: this.now(),
      ordinal: watch.ordinal
    })
    if (watch.stream.emit(observation, 128).terminated) {
      // The stream ended itself (overflow/error policy): forget the watch
      // and stop its reprobe without re-closing the stream.
      if (watch.reprobeTimer !== null) {
        clearTimeout(watch.reprobeTimer)
        watch.reprobeTimer = null
      }
      this.readinessWatches.delete(watch)
    } else if (ready) {
      if (watch.reprobeTimer !== null) {
        clearTimeout(watch.reprobeTimer)
        watch.reprobeTimer = null
      }
    } else this.scheduleReadinessReprobe(watch)
  }

  /**
   * The 100 ms readiness reprobe safety net (F2, legacy
   * `READINESS_REPROBE_DELAY_MS`): `peripheralIsReadyToSendWriteWithoutResponse`
   * is best-effort and an edge can be missed, so while the link stays unready
   * the probe re-runs on an interval — bounded by the caller's deadline and
   * signal, never past them. A failed re-read ends the watch `source-failed`,
   * as the legacy reprobe did.
   */
  private scheduleReadinessReprobe(watch: ReadinessWatch): void {
    if (watch.reprobeTimer !== null || watch.ready || !watch.probed) return
    if (!this.readinessWatches.has(watch) || this.destroyed) return
    watch.reprobeTimer = setTimeout(() => {
      watch.reprobeTimer = null
      this.reprobeReadiness(watch).catch(() => undefined)
    }, 100)
  }

  private async acceptReadinessReportsDuringProbe(watch: ReadinessWatch): Promise<void> {
    if (typeof this.central.takeWriteReadinessEvent !== 'function') return
    try {
      for (let count = 0; count <= 4096; count += 1) {
        const event = await this.central.takeWriteReadinessEvent()
        if (event == null) return
        if (event.kind === 'lagged') {
          this.closeReadinessWatch(
            watch,
            'source-failed',
            contractError('stream.overflow', 'stream', this.op('connection.write-readiness.concurrent-gap')).normalized
          )
          return
        }
        this.applyWriteReadiness(event)
      }
      this.closeReadinessWatch(
        watch,
        'source-failed',
        contractError('stream.quota', 'stream', this.op('connection.write-readiness.probe-fence')).normalized
      )
    } catch (error) {
      this.closeReadinessWatch(
        watch,
        'source-failed',
        desktopRustCoreError(error, this.op('connection.write-readiness.probe-fence')).normalized
      )
    }
  }

  private async reprobeReadiness(watch: ReadinessWatch): Promise<void> {
    if (!this.readinessWatches.has(watch) || this.destroyed || watch.ready || !watch.probed) return
    if (watch.signal?.aborted === true) return
    if (watch.deadline !== null && this.now() > watch.deadline) return
    if (watch.record.state !== 'connected') {
      this.closeReadinessWatch(watch, 'connection-lost')
      return
    }
    const revision = watch.acceptanceRevision
    const generation = watch.record.coreGeneration
    let ready: boolean
    try {
      ready = await this.central.writeReadiness({ peerId: watch.record.nativePeerId, lease: watch.record.lease })
      await this.acceptReadinessReportsDuringProbe(watch)
    } catch (error) {
      await this.acceptReadinessReportsDuringProbe(watch)
      if (
        this.readinessWatches.has(watch) &&
        watch.acceptanceRevision === revision &&
        watch.record.coreGeneration === generation
      ) {
        this.closeReadinessWatch(
          watch,
          'source-failed',
          desktopRustCoreError(error, this.op('connection.write-readiness.reconcile')).normalized
        )
      }
      return
    }
    if (
      this.readinessWatches.has(watch) &&
      watch.acceptanceRevision === revision &&
      watch.record.coreGeneration === generation &&
      watch.record.state === 'connected'
    )
      this.emitReadiness(watch, ready)
  }

  /**
   * Write-readiness reports were missed (N5): re-read each watched link's
   * readiness from the OS and report it; a link no longer connected ends
   * its watch `connection-lost`, a failed re-read ends it `source-failed`.
   */
  private async reconcileWriteReadiness(missed: number | null): Promise<void> {
    this.noteDiagnostic('write-readiness-events-lagged', 'write-readiness reports were missed; re-reading them', {
      missed
    })
    for (const watch of [...this.readinessWatches]) {
      if (this.destroyed) return
      if (watch.record.state !== 'connected') {
        this.closeReadinessWatch(watch, 'connection-lost')
        continue
      }
      const revision = watch.acceptanceRevision
      const generation = watch.record.coreGeneration
      let ready: boolean
      try {
        ready = await this.central.writeReadiness({ peerId: watch.record.nativePeerId, lease: watch.record.lease })
        await this.acceptReadinessReportsDuringProbe(watch)
      } catch (error) {
        await this.acceptReadinessReportsDuringProbe(watch)
        if (
          this.readinessWatches.has(watch) &&
          watch.acceptanceRevision === revision &&
          watch.record.coreGeneration === generation &&
          watch.record.state === 'connected'
        )
          this.closeReadinessWatch(
            watch,
            'source-failed',
            desktopRustCoreError(error, this.op('connection.write-readiness.reconcile')).normalized
          )
        continue
      }
      if (
        this.readinessWatches.has(watch) &&
        watch.acceptanceRevision === revision &&
        watch.record.coreGeneration === generation &&
        watch.record.state === 'connected'
      )
        this.emitReadiness(watch, ready)
    }
  }

  private applyWriteReadiness(event: DesktopRustCoreWriteReadinessEvent): void {
    if (event.kind === 'closed') {
      this.failCoreEventSource('write-readiness-events-closed')
      return
    }
    if (event.kind !== 'state') {
      this.noteDiagnostic(`write-readiness-events-${event.kind}`, 'write-readiness reports were missed', {
        missed: event.missed ?? null
      })
      return
    }
    for (const watch of [...this.readinessWatches]) {
      if (watch.record.nativePeerId !== event.peerId) continue
      const generation = typeof event.connectionGeneration === 'string' ? event.connectionGeneration : null
      const generationMatches = generation !== null && generation === watch.record.coreGeneration
      // Only an explicit matching native generation can accept an observation.
      if (!generationMatches) continue
      if (watch.record.state !== 'connected') {
        this.closeReadinessWatch(watch, 'connection-lost')
        continue
      }
      // F3: a report arriving before the probe resolves buffers (latest
      // wins) and replays after it — it is never dropped.
      this.emitReadiness(watch, event.ready === true)
    }
  }

  /**
   * The OS ended the scan without a stop request (legacy WinRT
   * `OnScanTerminal`). The core already settled it and released the owner:
   * every consumer ends — `source-failed` with the OS's words when the OS
   * reported an error, `closed` otherwise — and a new scan can start.
   */
  private async applyScanTerminal(event: DesktopRustCoreScanTerminalEvent): Promise<void> {
    if (event.kind === 'closed') {
      this.failCoreEventSource('scan-terminal-events-closed')
      return
    }
    if (event.kind !== 'terminal') {
      this.noteDiagnostic(`scan-terminal-events-${event.kind}`, 'the core scan-terminal stream closed', {
        missed: event.missed ?? null
      })
      return
    }
    const group = this.scanGroup
    if (group === null || group.coreOperationId !== event.operationId) return
    const owner = group.consumers.get(String(group.ownerLeaseId))
    this.scanGroup = null
    group.state = 'stopped'
    const error =
      event.aborted === true
        ? contractError('scan.start-failed', 'scan', this.op('scan.terminated'), {
            domain: 'desktop-rust-core',
            code: 'scan-terminated',
            safeMessage: (event.detail ?? '').slice(0, 512),
            metadata: Object.freeze({})
          }).normalized
        : null
    const reason = error === null ? 'closed' : 'source-failed'
    const evidence = await this.commitPendingTerminalEvidence(
      event.evidence,
      'scan',
      owner?.options.terminalEvidenceScope ?? null,
      Object.freeze({
        kind: 'terminal',
        reason,
        droppedItems: resourceCount(0),
        droppedBytes: resourceCount(0),
        replacedItems: resourceCount(0),
        ...(error === null ? {} : { error })
      }),
      this.op('scan.terminated')
    )
    for (const consumer of group.consumers.values()) {
      consumer.removeAbort()
      consumer.stream.closeWithReason(reason, error, consumer === owner ? evidence : undefined)
    }
    group.consumers.clear()
  }

  /**
   * Scan-terminal reports were missed (N5): re-read which scan the core
   * owns. A scan it no longer owns ended without this backend being told,
   * so every consumer ends `source-failed` (why it ended was in the missed
   * report) and a new scan can start; a scan it still owns stays open.
   */
  private reconcileScan(missed: number | null): void {
    this.noteDiagnostic('scan-terminal-events-lagged', 'scan terminal reports were missed; re-reading the owned scan', {
      missed
    })
    const group = this.scanGroup
    if (group === null || group.state !== 'active' || this.destroyed) return
    if (this.central.activeScanId() === group.coreOperationId) return
    this.scanGroup = null
    group.state = 'stopped'
    const error = contractError('scan.start-failed', 'scan', this.op('scan.terminated'), {
      domain: 'desktop-rust-core',
      code: 'scan-terminal-missed',
      safeMessage: 'the core no longer owns this scan and the report of how it ended was missed',
      metadata: Object.freeze({ missed })
    }).normalized
    for (const consumer of group.consumers.values()) {
      consumer.removeAbort()
      consumer.stream.closeWithReason('source-failed', error)
    }
    group.consumers.clear()
  }

  // -- maximum write length (gatt:maximum-write-length) ----------------------

  private connectionRecordFor(connectionId: string, connectionGeneration: string, operation: string): ConnectionRecord {
    const record = this.connectionsById.get(connectionId)
    if (record === undefined) throw contractError('connection.not-found', 'connection', operation)
    if (String(record.path.connectionGeneration) !== connectionGeneration || record.state !== 'connected') {
      throw contractError('connection.stale', 'connection', operation)
    }
    return record
  }

  private async measureWriteLength(
    record: ConnectionRecord,
    mode: 'with-response' | 'without-response',
    operation: string,
    control: DesktopRustCoreControl
  ): Promise<number> {
    // Measured on the link itself, as legacy CoreBluetooth read
    // `maximumWriteValueLength(for:)` on the peripheral: no discovered
    // database is needed (LEGACY-AUDIT-1 #65).
    const maximum = await this.central.connectionMaximumWriteLength({
      peerId: record.nativePeerId,
      lease: record.lease,
      withResponse: mode === 'with-response',
      ...control
    })
    if (!Number.isSafeInteger(maximum) || maximum < 1) {
      throw contractError('protocol.malformed', 'gatt', `${operation}.result`)
    }
    return maximum
  }

  private async observeMaximumWriteLength(
    input: MaximumWriteLengthFeatureInput
  ): Promise<MaximumWriteLengthFeatureOutput> {
    const operation = this.op('gatt.maximum-write-length')
    this.assertOperational(operation)
    // F9: an empty connection identity is a malformed argument, not a
    // missing connection, as the legacy backend validated it.
    if (
      input.connectionId.length === 0 ||
      input.connectionGeneration.length === 0 ||
      (input.mode !== 'with-response' && input.mode !== 'without-response')
    ) {
      throw contractError('argument.invalid', 'gatt', operation)
    }
    const record = this.connectionRecordFor(input.connectionId, input.connectionGeneration, operation)
    let maximum: number
    try {
      maximum = await this.measureWriteLength(record, input.mode, operation, {})
    } catch (error) {
      throw desktopRustCoreError(error, operation)
    }
    return Object.freeze({
      connectionId: input.connectionId,
      connectionGeneration: input.connectionGeneration,
      mode: input.mode,
      maximumWriteLength: maximum,
      observedAtMonotonicMs: Math.floor(this.now())
    })
  }

  private maximumWriteLength<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ConnectionMaximumWriteLengthRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionMaximumWriteLengthMeasurement<string, Operation>> {
    const operation = this.op('gatt.maximum-write-length')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<ConnectionMaximumWriteLengthMeasurement<string, Operation>> => {
      const maximum = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.measureWriteLength(record, request.mode, operation, { ticket, ...this.budget(request.operation) })
      )
      return Object.freeze({
        connectionId: connection.connectionId,
        connectionGeneration: connection.connectionGeneration,
        mode: request.mode,
        maximumWriteLength: maximum,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  // -- address targeting (peer:address-targeting) ----------------------------

  /**
   * Mint a connectable peer handle for an out-of-band address. The core
   * resolves it to a radio peer at connect time (BlueZ ConnectDevice /
   * the OS device object); a malformed address fails there with
   * `argument.invalid` before any radio effect.
   */
  private peerFromAddress(descriptor: PeerAddressDescriptor): PeerId<string> {
    this.assertOperational(this.op('peer.address-targeting'))
    const peerId = this.mintPeer()
    this.pendingAddresses.set(String(peerId), Object.freeze({ ...descriptor }))
    return peerId
  }

  private async nativeIdForConnect(
    peerId: PeerId<string>,
    options: ConnectionOptions,
    operation: string
  ): Promise<string> {
    const known = this.nativeIdsByPeerId.get(String(peerId))
    if (known !== undefined) return known
    const pending = this.pendingAddresses.get(String(peerId))
    // W-R3: a never-observed peer is `peer.not-found` on every backend
    // (event `peer-not-found`); the legacy `connection.not-found` /
    // `winrt.connect.peer` identity is gone. The op keeps its legacy
    // shape — the connect op on BlueZ, with the `.peer` segment on
    // CoreBluetooth and WinRT.
    if (pending === undefined) {
      throw contractError(
        'peer.not-found',
        'connection',
        this.profile.platform === 'bluez' ? operation : `${operation}.peer`
      )
    }
    const correlation = String(this.mintedCorrelation())
    const nativePeerId = await this.withTicket(
      correlation,
      options.signal,
      `${this.op('peer.address-targeting')}`,
      ticket =>
        this.central.resolveAddress({
          address: pending.address,
          addressType: pending.addressType,
          ticket,
          ...this.budget(options)
        })
    )
    this.pendingAddresses.delete(String(peerId))
    this.peerIdsByNativeId.set(nativePeerId, peerId)
    this.nativeIdsByPeerId.set(String(peerId), nativePeerId)
    this.addressTypes.set(nativePeerId, pending.addressType)
    return nativePeerId
  }

  // -- security (security:state / pair / cancel-pairing / unpair) -------------

  private securityState(state: DesktopRustCoreSecurityState): PeerSecurityState {
    return Object.freeze({
      bond: state.bond,
      // No desktop OS adapter measures these; the legacy backends reported
      // them `unsupported` too.
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: typeof state.pairingPossible === 'boolean' ? state.pairingPossible : null,
      measuredAtMonotonicMs: this.now(),
      limitations: Object.freeze([
        Object.freeze({
          code: `${this.profile.platform}-rust-core-security-measurement`,
          explanation:
            'The OS reports bond state and whether pairing is possible; encryption, authentication and Secure Connections are not measured.',
          affectedGuarantee: 'security measurement completeness'
        })
      ])
    })
  }

  private nativePeerForSecurity(peerId: string, operation: string): string {
    const native = this.nativeIdsByPeerId.get(peerId)
    if (native === undefined) throw contractError('peer.not-found', 'connection', operation)
    return native
  }

  private createSecurityBackend(): SecurityBackend {
    const operation = (name: string) => this.op(`security.${name}`)
    return Object.freeze({
      state: async (peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> => {
        this.assertOperational(operation('state'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('state'))
        const correlation = String(this.mintedCorrelation())
        const state = await this.withTicket(correlation, options.signal, operation('state'), ticket =>
          this.central.securityState({ peerId: nativePeerId, ticket, ...this.budget(options) })
        )
        return this.securityState(state)
      },
      watch: (peerId: string): BoundedAsyncStream<PeerSecurityEvent> => {
        this.assertOperational(operation('watch'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('watch'))
        const stream: OwnedCoreBoundedStream<PeerSecurityEvent> = new OwnedCoreBoundedStream<PeerSecurityEvent>(
          { itemCapacity: capacity(16), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
          'drop-oldest',
          () => this.securityWatches.get(peerId)?.delete(stream)
        )
        const watches = this.securityWatches.get(peerId) ?? new Set()
        watches.add(stream)
        this.securityWatches.set(peerId, watches)
        // A watch opens with the peer's current state, as the 4.x WinRT and
        // BlueZ watches did; a state the OS cannot read ends it `source-failed`.
        this.central.securityState({ peerId: nativePeerId }).then(
          state => {
            // The read has no source revision. A matching observation already
            // delivered to this watch wins over its unordered admission snapshot;
            // another peer/watch must not suppress this watch's initial state.
            if (!this.securityWatches.get(peerId)?.has(stream) || this.securityObservedWatches.has(stream)) return
            const record: PeerSecurityEvent = Object.freeze({
              kind: 'state',
              peerId,
              // A current-state snapshot is a delivered observation too. Its
              // ordering shares the provider's sequence with subsequent events,
              // rather than borrowing the native event counter's initial zero.
              sequence: ++this.securitySequence,
              state: this.securityState(state)
            })
            if (stream.emit(record, 256).terminated) watches.delete(stream)
          },
          (error: unknown) => {
            stream.closeWithReason('source-failed', desktopRustCoreError(error, operation('watch')).normalized)
            watches.delete(stream)
          }
        )
        return stream
      },
      pair: async (peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> => {
        this.assertOperational(operation('pair'))
        if (options.ceremony !== 'system') {
          throw contractError('capability.unsupported', 'capability', operation('custom-ceremony'))
        }
        if (options.protection !== 'system-default') {
          throw contractError('capability.unsupported', 'capability', operation('pair.protection'))
        }
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('pair'))
        const secureConnections =
          options.secureConnections === undefined || options.secureConnections === 'prefer'
            ? {}
            : { secureConnections: options.secureConnections }
        const correlation = String(this.mintedCorrelation())
        const outcome = await this.withTicket(correlation, options.signal, operation('pair'), ticket =>
          this.central.pair({ peerId: nativePeerId, ticket, ...secureConnections, ...this.budget(options) })
        )
        return this.pairResult(outcome, operation('pair'))
      },
      cancelPairing: async (peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult> => {
        this.assertOperational(operation('cancel-pairing'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('cancel-pairing'))
        const correlation = String(this.mintedCorrelation())
        const answer = await this.withTicket(correlation, options.signal, operation('cancel-pairing'), ticket =>
          this.central.cancelPairing({ peerId: nativePeerId, ticket, ...this.budget(options) })
        )
        if (answer.outcome === 'rejected') return Object.freeze({ outcome: 'rejected', reason: answer.reason ?? null })
        return Object.freeze({ outcome: answer.outcome })
      },
      unpair: async (peerId: string, options: PublicOperationOptions): Promise<SecurityUnpairResult> => {
        this.assertOperational(operation('unpair'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('unpair'))
        const correlation = String(this.mintedCorrelation())
        const outcome = await this.withTicket(correlation, options.signal, operation('unpair'), ticket =>
          this.central.unpair({ peerId: nativePeerId, ticket, ...this.budget(options) })
        )
        return Object.freeze({ outcome })
      },
      close: () => {
        for (const streams of this.securityWatches.values()) {
          for (const stream of streams) stream.closeWithReason('owner-released')
        }
        this.securityWatches.clear()
      }
    })
  }

  private pairResult(outcome: DesktopRustCorePairOutcome, operation: string): SecurityPairResult {
    if (outcome.outcome === 'rejected') return Object.freeze({ outcome: 'rejected', reason: outcome.reason ?? null })
    if (outcome.outcome === 'cancelled') return Object.freeze({ outcome: 'cancelled' })
    if (outcome.state === null || outcome.state === undefined) {
      throw contractError('protocol.malformed', 'platform', `${operation}.state`)
    }
    return Object.freeze({ outcome: outcome.outcome, state: this.securityState(outcome.state) })
  }

  /**
   * Link-security events were missed (N5): re-read each watched peer's
   * security state from the OS and deliver it. Its sequence is the end of
   * the gap (last delivered + missed), so later core events still order
   * after it; a failed re-read ends that peer's watches `source-failed`.
   */
  private async reconcileSecurity(missed: number | null): Promise<void> {
    this.noteDiagnostic('security-events-lagged', 'link-security events were missed; re-reading security state', {
      missed
    })
    const sequence = this.securitySequence + (missed ?? 1)
    this.securitySequence = sequence
    for (const [peerId, watches] of [...this.securityWatches]) {
      if (this.destroyed) return
      let state: DesktopRustCoreSecurityState
      try {
        state = await this.central.securityState({
          peerId: this.nativePeerForSecurity(peerId, this.op('security.reconcile'))
        })
      } catch (error) {
        const normalized = desktopRustCoreError(error, this.op('security.reconcile')).normalized
        for (const stream of watches) stream.closeWithReason('source-failed', normalized)
        this.securityWatches.delete(peerId)
        continue
      }
      this.deliverSecurityState(peerId, sequence, state)
    }
  }

  private deliverSecurityState(peerId: string, sequence: number, state: DesktopRustCoreSecurityState): void {
    const watches = this.securityWatches.get(peerId)
    if (watches === undefined) return
    const record: PeerSecurityEvent = Object.freeze({
      kind: 'state',
      peerId,
      sequence,
      state: this.securityState(state)
    })
    for (const stream of [...watches]) {
      this.securityObservedWatches.add(stream)
      if (stream.emit(record, 256).terminated) watches.delete(stream)
    }
  }

  private applySecurityEvent(event: DesktopRustCoreSecurityEvent): void {
    if (event.kind === 'closed') {
      this.failCoreEventSource('security-events-closed')
      return
    }
    if (event.kind !== 'state') {
      this.noteDiagnostic(`security-events-${event.kind}`, 'the core link-security stream closed', {
        missed: event.missed ?? null
      })
      return
    }
    if (typeof event.peerId !== 'string' || event.state === null || event.state === undefined) return
    const peerId = this.peerIdsByNativeId.get(event.peerId)
    if (peerId === undefined) return
    this.deliverSecurityState(String(peerId), ++this.securitySequence, event.state)
  }

  // -- connections -----------------------------------------------------------

  private async connect(
    peerId: PeerId<string>,
    _clientId: ClientId<string, string>,
    options: ConnectionOptions
  ): Promise<ConnectionLease<string, string, string>> {
    const operation = this.op('connection.connect')
    this.assertOperational(operation)
    // The core connects directly over LE: intents, transports and PHY
    // selections it cannot express fail closed before any dispatch.
    // F5: the when-available intent keeps its legacy operation id.
    if (
      options.intent !== undefined &&
      options.intent !== 'direct' &&
      !(options.intent === 'when-available' && this.wiring.whenAvailable)
    ) {
      throw contractError(
        'capability.unsupported',
        'connection',
        options.intent === 'when-available' ? `${operation}.when-available` : `${operation}.intent`
      )
    }
    if (options.transport !== undefined && options.transport !== 'le' && options.transport !== 'auto') {
      throw contractError('argument.invalid', 'connection', `${operation}.transport`)
    }
    if (options.preferredPhy !== undefined && options.preferredPhy.length > 0) {
      throw contractError('capability.unsupported', 'connection', `${operation}.phy`)
    }
    const nativePeerId = await this.nativeIdForConnect(peerId, options, operation)
    await this.supersedePendingAcquisition(nativePeerId)
    // Same-peer join (UNIFIED_SEMANTICS §3/§8, Android reference): a live
    // link is leased, never re-dialled — the second lease shares the peer's
    // native link with an independent generation. Only a peer with no live
    // link reaches the radio.
    const shared = this.liveSharedLink(nativePeerId)
    if (shared !== null) {
      return this.joinSharedLink(peerId, shared)
    }
    const ordinal = this.nextOrdinal()
    const lease = `${this.profile.platform}-core-lease-${ordinal}`
    const correlation = String(this.mintedCorrelation())
    const connected = await this.withTicket(correlation, options.signal, operation, ticket => {
      const acquisition =
        options.intent === 'when-available'
          ? this.central.connectWhenAvailable({ peerId: nativePeerId, lease, ticket, ...this.budget(options) })
          : this.central.connect({ peerId: nativePeerId, lease, ticket, ...this.budget(options) })
      this.pendingAcquisitions.set(nativePeerId, {
        ticket,
        settled: acquisition.then(
          () => undefined,
          () => undefined
        )
      })
      return acquisition.finally(() => {
        if (this.pendingAcquisitions.get(nativePeerId)?.ticket === ticket) {
          this.pendingAcquisitions.delete(nativePeerId)
        }
      })
    })
    if (typeof connected.peerKey !== 'string' || typeof connected.connectionGeneration !== 'string') {
      throw contractError('protocol.malformed', 'core', `${operation}.shape`)
    }
    const scope = `${this.profile.platform}-rust-core`
    // Legacy numbered the link after the core admitted it; the public
    // generation is this host's legacy name for the core's `coreGeneration`.
    const linkOrdinal = this.nextConnection
    this.nextConnection += 1
    const platform = this.profile.platform
    const connectionId = this.identifiers.connectionId(`${platform}-connection-${linkOrdinal}`)
    const leaseId = this.identifiers.leaseId(`${platform}-connection-lease-${linkOrdinal}`)
    const connectionGeneration = opaqueId(
      LEGACY_DESKTOP_NAMES[platform].connectionGeneration(linkOrdinal),
      'connection-generation',
      scope
    )
    const path: ConnectionPath<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      ownerLeaseId: leaseId,
      connectionGeneration
    })
    const record: ConnectionRecord = {
      nativePeerId,
      lease,
      coreGeneration: connected.connectionGeneration,
      path,
      state: 'connected',
      databases: new Set(),
      subscriptions: new Set(),
      acquiredClosers: new Set(),
      nextDatabase: 1
    }
    this.connectionsById.set(String(connectionId), record)
    const release = (): Promise<BackendConnectionCleanupRecord> => this.disconnectConnection(record)
    const connection: BackendConnection<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      connectionGeneration,
      state: 'connected',
      disconnect: () => this.disconnectConnection(record, true)
    })
    return Object.freeze({ leaseId, connection, release })
  }

  /**
   * One live native link per peer, shared by leases (UNIFIED_SEMANTICS §3/§8,
   * Android reference). A connect joins the peer's `connected` link with an
   * independent generation instead of re-dialling the radio; a peer whose
   * links are all tearing down arbitrates `connection.already-owned` until
   * the teardown completes, and the next connect dials a fresh link.
   */
  private liveSharedLink(nativePeerId: string): ConnectionRecord | null {
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId !== nativePeerId) continue
      if (record.state === 'connected') return record
    }
    return null
  }

  private async joinSharedLink(
    peerId: PeerId<string>,
    shared: ConnectionRecord
  ): Promise<ConnectionLease<string, string, string>> {
    const platform = this.profile.platform
    const lease = `${platform}-core-lease-${this.nextOrdinal()}`
    if (typeof this.central.retainConnectionLease !== 'function')
      throw contractError('protocol.incompatible', 'boundary', this.op('connection.connect'))
    try {
      await this.central.retainConnectionLease({
        peerId: shared.nativePeerId,
        sourceLease: shared.lease,
        destinationLease: lease,
        connectionGeneration: shared.coreGeneration
      })
    } catch (error) {
      throw desktopRustCoreError(error, this.op('connection.connect'))
    }
    const scope = `${platform}-rust-core`
    const linkOrdinal = this.nextConnection
    this.nextConnection += 1
    const connectionId = this.identifiers.connectionId(`${platform}-connection-${linkOrdinal}`)
    const leaseId = this.identifiers.leaseId(`${platform}-connection-lease-${linkOrdinal}`)
    const connectionGeneration = opaqueId(
      LEGACY_DESKTOP_NAMES[platform].connectionGeneration(linkOrdinal),
      'connection-generation',
      scope
    )
    const path: ConnectionPath<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      ownerLeaseId: leaseId,
      connectionGeneration
    })
    const record: ConnectionRecord = {
      nativePeerId: shared.nativePeerId,
      lease,
      coreGeneration: shared.coreGeneration,
      path,
      state: 'connected',
      databases: new Set(),
      subscriptions: new Set(),
      acquiredClosers: new Set(),
      nextDatabase: 1
    }
    this.connectionsById.set(String(connectionId), record)
    const release = (): Promise<BackendConnectionCleanupRecord> => this.disconnectConnection(record)
    const connection: BackendConnection<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      connectionGeneration,
      state: 'connected',
      disconnect: () => this.disconnectConnection(record, true)
    })
    return Object.freeze({ leaseId, connection, release })
  }

  /**
   * Cancel this provider's in-flight acquisition for `nativePeerId`, if
   * any, and wait for it to settle (finding 194). The superseded caller
   * already holds its terminal answer — it moved on, which is why a newer
   * connect is here. Cancelling reaps the core's `Connecting` claim, so
   * the new connect arbitrates against a terminal record. Best-effort: a
   * cancel that cannot land leaves today's arbitration answer in place,
   * and the failure is reported on the background channel, never
   * swallowed. A live established link is untouched: only a pending
   * acquisition is ever superseded.
   */
  private async supersedePendingAcquisition(nativePeerId: string): Promise<void> {
    if (this.profile.platform === 'bluez') return
    const pending = this.pendingAcquisitions.get(nativePeerId)
    if (pending === undefined) return
    this.pendingAcquisitions.delete(nativePeerId)
    try {
      await this.central.cancelTicket(pending.ticket)
    } catch (error) {
      this.noteBackgroundFailure('connect-supersede-cancel-failed', error)
      return
    }
    await pending.settled
  }

  /**
   * Release through the core: only the OS answer releases the link. A lease
   * that shares its peer's link with other live leases ends locally without
   * radio work; the final holder drives the radio. A failed or timed-out
   * release keeps the link `disconnecting` in the core and reports
   * `release-failed`; a retry drives the radio again, and a link that
   * already ended answers `already-released` without a radio call.
   */
  private disconnectConnection(
    record: ConnectionRecord,
    requestedDisconnect = false
  ): Promise<BackendConnectionCleanupRecord> {
    if (record.releaseAttempt !== undefined) return record.releaseAttempt
    record.requestedDisconnect = requestedDisconnect
    const attempt = this.disconnectConnectionCurrent(record)
    record.releaseAttempt = attempt
    const settled = () => {
      if (record.releaseAttempt === attempt) record.releaseAttempt = undefined
    }
    attempt.then(settled, settled)
    return attempt
  }

  private async disconnectConnectionCurrent(record: ConnectionRecord): Promise<BackendConnectionCleanupRecord> {
    const operation = this.op('connection.disconnect')
    if (record.state === 'connected') record.state = 'disconnecting'
    const childFailures = []
    for (const close of [...record.acquiredClosers]) {
      const cleanup = await close()
      childFailures.push(...cleanup.failures)
    }
    if (childFailures.length > 0) {
      return Object.freeze({ state: 'release-failed', failures: Object.freeze(childFailures) })
    }
    if (!['connected', 'disconnecting'].includes(record.state)) {
      // Logical loss does not retire native discovery/session cleanup debt.
      // The core visits only the exact terminal owner without disconnecting
      // a newer live generation; refusal keeps this public retry route alive.
      const sameGenerationStillLive = [...this.connectionsById.values()].some(
        candidate =>
          candidate !== record &&
          candidate.nativePeerId === record.nativePeerId &&
          candidate.coreGeneration === record.coreGeneration &&
          (candidate.state === 'connected' || candidate.state === 'disconnecting')
      )
      // A terminal owner is already locally retired. If a sibling lease still
      // owns this native generation, sending the retired owner's release to
      // the core would make the terminal cleanup compete with that live
      // owner. The surviving lease is the only route that may release this
      // generation; keep the terminal record's completed cleanup idempotent.
      if (sameGenerationStillLive) {
        return Object.freeze({ state: 'released', failures: Object.freeze([]) })
      }
      try {
        const answer = await this.central.disconnect({ peerId: record.nativePeerId, lease: record.lease })
        record.releasePlatform = parseDesktopRustCoreReleaseReport(answer, {
          peerId: record.nativePeerId,
          lease: record.lease,
          connectionGeneration: record.coreGeneration
        })
      } catch (error) {
        return Object.freeze({
          state: 'release-failed',
          failures: Object.freeze([
            { resourceKind: 'connection', error: desktopRustCoreError(error, operation).normalized }
          ])
        })
      }
      return Object.freeze({
        state: 'released',
        failures: Object.freeze([]),
        ...(record.releasePlatform === undefined ? {} : { platform: record.releasePlatform })
      })
    }
    try {
      const answer = await this.central.disconnect({ peerId: record.nativePeerId, lease: record.lease })
      record.releasePlatform = parseDesktopRustCoreReleaseReport(answer, {
        peerId: record.nativePeerId,
        lease: record.lease,
        connectionGeneration: record.coreGeneration
      })
    } catch (error) {
      const normalized = desktopRustCoreError(error, operation).normalized
      return Object.freeze({
        state: 'release-failed',
        failures: Object.freeze([{ resourceKind: 'connection', error: normalized }])
      })
    }
    record.state = 'disconnected'
    this.invalidateConnectionState(record, 'connection-lost')
    if (record.requestedDisconnect === true) this.emitRequestedDisconnect(record)
    return Object.freeze({
      state: 'released',
      failures: Object.freeze([]),
      ...(record.releasePlatform === undefined ? {} : { platform: record.releasePlatform })
    })
  }

  private liveConnection(connection: BackendConnection<string, string>, operation: string): ConnectionRecord {
    const record = this.connectionsById.get(String(connection.connectionId))
    if (record === undefined) {
      throw contractError('connection.not-found', 'connection', operation)
    }
    if (record.state !== 'connected') {
      throw contractError('connection.stale', 'connection', operation)
    }
    return record
  }

  private readRssi<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadRssiRequest<string, Operation>
  ): BackendOperationDispatch<string, RssiMeasurement<string, Operation>> {
    const operation = this.op('connection.rssi')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<RssiMeasurement<string, Operation>> => {
      const rssi = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.readRssi({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(request.operation)
        })
      )
      return Object.freeze({
        rssi,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  private readPhy<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadPhyRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPhyObservation<string, Operation>> {
    const operation = this.op('connection.read-phy')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<ConnectionPhyObservation<string, Operation>> => {
      const result = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.readPhy({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(request.operation)
        })
      )
      const valid = (value: unknown): value is 'le-1m' | 'le-2m' | 'le-coded' =>
        value === 'le-1m' || value === 'le-2m' || value === 'le-coded'
      if (!valid(result.txPhy) || !valid(result.rxPhy))
        throw contractError('protocol.violation', 'connection', operation)
      return Object.freeze({
        txPhy: result.txPhy,
        rxPhy: result.rxPhy,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  private requestPriority<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestPriorityRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPriorityRequest<string, Operation>> {
    const operation = this.op('connection.request-priority')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<ConnectionPriorityRequest<string, Operation>> => {
      const accepted = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.requestPriority({
          peerId: record.nativePeerId,
          lease: record.lease,
          priority: request.priority,
          ticket,
          ...this.budget(request.operation)
        })
      )
      if (typeof accepted !== 'boolean')
        throw contractError('protocol.violation', 'connection', operation, {
          domain: 'desktop-rust-core',
          code: 'malformed-priority-acceptance',
          safeMessage: 'Native priority request returned a malformed acceptance',
          metadata: {}
        })
      return Object.freeze({
        requested: request.priority,
        accepted,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  private effectiveMtu<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: EffectiveMtuRequest<string, Operation>
  ): BackendOperationDispatch<string, EffectiveMtuMeasurement<string, Operation>> {
    const operation = this.op('connection.effective-mtu')
    this.assertOperational(operation)
    const record = this.liveConnection(connection, operation)
    const correlation = String(request.operation.correlation)
    const completion = (async (): Promise<EffectiveMtuMeasurement<string, Operation>> => {
      const mtu = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.readEffectiveMtu({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(request.operation)
        })
      )
      return Object.freeze({
        connectionId: connection.connectionId,
        connectionGeneration: connection.connectionGeneration,
        attMtu: mtu,
        payloadBytes: mtu === null ? null : mtu - 3,
        platformPduBytes: null,
        observedAtMonotonicMs: this.now(),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, completion)
  }

  // -- GATT ------------------------------------------------------------------

  private mintOccurrence(kind: string, numeral: number): string {
    // Occurrence identities are decimal strings of the core numeral (the
    // portable snapshot layer requires `/^(0|[1-9][0-9]*)$/`): the brand
    // carries scope, the value stays the numeral.
    const id = String(opaqueId(String(numeral), kind, `${this.profile.platform}-rust-core`))
    this.occurrenceNumerals.set(id, numeral)
    return id
  }

  private occurrenceNumeral(id: unknown, operation: string): number {
    const numeral = this.occurrenceNumerals.get(String(id))
    if (numeral === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    return numeral
  }

  private selectorFor(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): DesktopRustCoreSelector {
    return {
      serviceUuid: String(path.serviceUuid),
      serviceOccurrence: this.occurrenceNumeral(path.serviceOccurrence, `${operation}.service-occurrence`),
      characteristicUuid: String(path.characteristicUuid),
      characteristicOccurrence: this.occurrenceNumeral(
        path.characteristicOccurrence,
        `${operation}.characteristic-occurrence`
      )
    }
  }

  private descriptorSelectorFor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): DesktopRustCoreSelector {
    return {
      ...this.selectorFor(path, operation),
      descriptorUuid: String(path.descriptorUuid),
      descriptorOccurrence: this.occurrenceNumeral(path.descriptorOccurrence, `${operation}.descriptor-occurrence`)
    }
  }

  private connectionForPath(
    path: { readonly connectionId: ConnectionId<string, string> },
    operation: string
  ): ConnectionRecord {
    const record = this.connectionsById.get(String(path.connectionId))
    if (record === undefined) {
      throw contractError('peer.not-found', 'connection', operation)
    }
    return record
  }

  private reserveOrderedRecordingMarker(base: DatabasePath<string, string, string>, recordingId: string) {
    const operation = this.op('gatt.ordered-recording-marker')
    this.assertOperational(operation)
    if (!isRecordingId(recordingId)) throw contractError('argument.invalid', 'gatt', operation)
    if (
      !this.processOwnedRecording ||
      this.central.orderedMarkerWireVersion?.call(this.central) !== 2 ||
      this.central.orderedMarkerWriteSupported?.call(this.central) !== true ||
      !this.central.reserveRecordingMarkerWrite ||
      !this.central.startRecordingMarkerWrite ||
      !this.central.recordingMarkerWriteStatus ||
      !this.central.cancelRecordingMarkerWrite ||
      !this.central.forgetRecordingMarkerWrite
    )
      throw contractError('capability.unsupported', 'gatt', operation)
    const session = this.orderedRecorderSessions.get(recordingId)
    if (!session || !session.active || session.databaseId !== String(base.databaseId))
      throw contractError('ownership.denied', 'gatt', operation)
    const record = this.connectionForPath(base, operation)
    if (record.state !== 'connected') throw contractError('gatt.stale-handle', 'gatt', operation)
    const central = this.central
    const markerTransport: Pick<
      DesktopRustCoreCentral,
      | 'reserveOrderedMarkerWrite'
      | 'startOrderedMarkerWrite'
      | 'orderedMarkerWriteStatus'
      | 'cancelOrderedMarkerWrite'
      | 'forgetOrderedMarkerWrite'
    > = {
      reserveOrderedMarkerWrite: options => {
        const invoke = central.reserveRecordingMarkerWrite
        if (!invoke) throw contractError('capability.unsupported', 'gatt', operation)
        return invoke.call(central, {
          wireVersion: 2,
          recordingId,
          peerId: record.nativePeerId,
          lease: record.lease,
          reservationKey: options.reservationKey
        })
      },
      startOrderedMarkerWrite: options => {
        const invoke = central.startRecordingMarkerWrite
        if (!invoke) throw contractError('capability.unsupported', 'gatt', operation)
        return invoke.call(central, {
          wireVersion: 2,
          recordingId,
          peerId: record.nativePeerId,
          lease: record.lease,
          reservationId: options.reservationId,
          selector: options.selector,
          value: options.value,
          withResponse: options.withResponse
        })
      },
      orderedMarkerWriteStatus: options => {
        const invoke = central.recordingMarkerWriteStatus
        if (!invoke) throw contractError('capability.unsupported', 'gatt', operation)
        return invoke.call(central, {
          wireVersion: 2,
          recordingId,
          peerId: record.nativePeerId,
          lease: record.lease,
          reservationId: options.reservationId
        })
      },
      cancelOrderedMarkerWrite: options => {
        const invoke = central.cancelRecordingMarkerWrite
        if (!invoke) throw contractError('capability.unsupported', 'gatt', operation)
        return invoke.call(central, {
          wireVersion: 2,
          recordingId,
          peerId: record.nativePeerId,
          lease: record.lease,
          reservationId: options.reservationId
        })
      },
      forgetOrderedMarkerWrite: options => {
        const invoke = central.forgetRecordingMarkerWrite
        if (!invoke) throw contractError('capability.unsupported', 'gatt', operation)
        return invoke.call(central, {
          wireVersion: 2,
          recordingId,
          peerId: record.nativePeerId,
          lease: record.lease,
          reservationId: options.reservationId
        })
      }
    }
    return desktopMarkerReservations(
      markerTransport,
      { wireVersion: 2, peerId: '', collectionId: '' },
      (target: CharacteristicPath<string, string, string, string, string, 'current'>) =>
        this.selectorFor(this.resolveCharacteristic(base, target, operation), operation),
      () => this.now(),
      () => String(this.mintedCorrelation())
    )()
  }

  private reserveOrderedRecordingReclaim(
    base: DatabasePath<string, string, string>,
    handoff: OrderedRecordingRecoveryHandle
  ) {
    const operation = this.op('gatt.ordered-recording-reclaim')
    if (this.central.orderedRecordingReclaimSupported?.call(this.central) !== true)
      throw contractError('capability.unsupported', 'gatt', operation)
    if (this.central.orderedRecordingReclaimWireVersion?.call(this.central) !== 2)
      throw contractError('protocol.incompatible', 'boundary', operation)
    const authority = this.orderedRecoveryHandles.get(handoff)
    const invoke = this.central.orderedRecordingReclaim
    const confirm = this.central.confirmOrderedRecordingReclaim
    if (!authority || !invoke || !confirm) throw contractError('ownership.denied', 'gatt', operation)
    const session = this.orderedRecorderSessions.get(authority.recordingId)
    const record = this.connectionForPath(base, operation)
    if (
      !session ||
      session.startHandle !== authority.startHandle ||
      !session.active ||
      !session.recovery ||
      record.nativePeerId !== session.peerId ||
      record.state !== 'connected'
    )
      throw contractError('ownership.denied', 'gatt', operation)
    if (authority.busy) throw contractError('lifecycle.invalid-state', 'cleanup', operation)
    let recovery: unknown
    try {
      recovery = JSON.parse(session.recovery)
    } catch {
      throw contractError('protocol.malformed', 'boundary', operation)
    }
    if (typeof recovery !== 'object' || recovery === null)
      throw contractError('protocol.malformed', 'boundary', operation)
    const capability: unknown = Reflect.get(recovery, 'capability')
    if (typeof capability !== 'string') throw contractError('protocol.malformed', 'boundary', operation)
    const call = async (
      action:
        | 'next'
        | 'reserve'
        | 'start'
        | 'status'
        | 'commit'
        | 'cancel'
        | 'release'
        | 'retire'
        | 'ack-retirement'
        | 'forget-retirement',
      callerKey: string,
      requestId?: string
    ) => {
      try {
        return await invoke.call(this.central, action, {
          wireVersion: 2,
          recordingId: authority.recordingId,
          capability,
          lease: record.lease,
          callerKey,
          ...(requestId === undefined ? {} : { requestId })
        })
      } catch (error) {
        throw desktopRustCoreError(error, operation)
      }
    }
    const receipt = async (
      action: 'reserve' | 'start' | 'status' | 'commit' | 'cancel' | 'release',
      id: string,
      key: string
    ): Promise<OrderedRecordingReclaimReceipt> => {
      let raw: unknown
      try {
        raw = JSON.parse(await call(action, key, id))
      } catch (error) {
        if (error instanceof BackendContractError) throw error
        throw contractError('protocol.malformed', 'boundary', operation)
      }
      if (
        typeof raw !== 'object' ||
        raw === null ||
        Array.isArray(raw) ||
        Object.keys(raw).sort().join(',') !== 'error,mappingCommitted,state'
      )
        throw contractError('protocol.malformed', 'boundary', operation)
      const state: unknown = Reflect.get(raw, 'state')
      const committed: unknown = Reflect.get(raw, 'mappingCommitted')
      const failure: unknown = Reflect.get(raw, 'error')
      if (
        !(
          state === 'reserved' ||
          state === 'pending' ||
          state === 'ready' ||
          state === 'committed' ||
          state === 'releasing' ||
          state === 'release-failed' ||
          state === 'released' ||
          state === 'failed' ||
          state === 'retired'
        ) ||
        typeof committed !== 'boolean' ||
        (state === 'failed' || state === 'release-failed' ? typeof failure !== 'string' : failure !== null) ||
        (typeof failure === 'string' && parseDesktopRustCoreWireError(failure) === null) ||
        (state === 'committed' && !committed) ||
        (['reserved', 'pending', 'ready', 'failed'].includes(state) && committed)
      )
        throw contractError('protocol.malformed', 'boundary', operation)
      const result: OrderedRecordingReclaimReceipt = {
        state,
        mappingCommitted: committed,
        error: typeof failure === 'string' ? desktopRustCoreError(new Error(failure), operation).normalized : null
      }
      if (committed && this.orderedRecorderSessions.get(authority.recordingId) === session)
        session.databaseId = String(base.databaseId)
      return result
    }
    authority.busy = true
    const reserve = createOrderedRecordingReclaimReservations<OrderedRecordingReclaimReceipt>(
      {
        next: async key => {
          const response = await call('next', key)
          if (isCanonicalIngressOrdinal(response)) return response
          let raw: unknown
          try {
            raw = JSON.parse(response)
          } catch {
            throw contractError('protocol.malformed', 'boundary', operation)
          }
          if (
            typeof raw !== 'object' ||
            raw === null ||
            Array.isArray(raw) ||
            Object.keys(raw).sort().join(',') !== 'error,state' ||
            Reflect.get(raw, 'state') !== 'not-reserved'
          )
            throw contractError('protocol.malformed', 'boundary', operation)
          const failure: unknown = Reflect.get(raw, 'error')
          if (typeof failure !== 'string' || parseDesktopRustCoreWireError(failure) === null)
            throw contractError('protocol.malformed', 'boundary', operation)
          return { state: 'not-reserved', error: desktopRustCoreError(new Error(failure), operation).normalized }
        },
        reserve: async (id, key) => {
          if ((await receipt('reserve', id, key)).state !== 'reserved')
            throw contractError('protocol.malformed', 'boundary', operation)
        },
        start: (id, key) => receipt('start', id, key),
        status: (id, key) => receipt('status', id, key),
        commit: (id, key) => receipt('commit', id, key),
        cancel: (id, key) => receipt('cancel', id, key),
        release: (id, key) => receipt('release', id, key),
        retire: async (id, key) => {
          if ((await call('retire', key, id)) !== '{"state":"retired"}')
            throw contractError('protocol.malformed', 'boundary', operation)
        },
        acknowledgeRetirement: async (id, key) => {
          if ((await call('ack-retirement', key, id)) !== '{"state":"acknowledged"}')
            throw contractError('protocol.malformed', 'boundary', operation)
        },
        forgetRetirement: async (id, key) => {
          if ((await call('forget-retirement', key, id)) !== '{"state":"forgotten"}')
            throw contractError('protocol.malformed', 'boundary', operation)
          confirm.call(this.central, {
            wireVersion: 2,
            recordingId: authority.recordingId,
            capability,
            lease: record.lease,
            callerKey: key,
            requestId: id
          })
        }
      },
      () => String(this.mintedCorrelation())
    )
    const handle = reserve()
    return Object.freeze({
      ...handle,
      retire: async () => {
        await handle.retire()
        authority.busy = false
      }
    })
  }

  private reserveOrderedRecordingStart(base: DatabasePath<string, string, string>) {
    const operation = this.op('gatt.ordered-recording')
    this.assertOperational(operation)
    if (!this.processOwnedRecording) throw contractError('capability.unsupported', 'gatt', operation)
    if (this.orderedRecorderSessions.size >= 256) throw contractError('lifecycle.invalid-state', 'cleanup', operation)
    const reserve = this.central.reserveOrderedRecordingStart
    const start = this.central.startOrderedRecording
    const status = this.central.orderedRecordingStartStatus
    const retire = this.central.retireOrderedRecordingStart
    const issueRecovery = this.central.issueOrderedRecordingRecovery
    if (
      !reserve ||
      !start ||
      !status ||
      !retire ||
      !issueRecovery ||
      !this.central.forgetOrderedRecordingSession ||
      this.central.orderedGroupWireVersion?.call(this.central) !== 5 ||
      this.central.orderedMetadataVersion?.call(this.central) !== 2
    )
      throw contractError('capability.unsupported', 'gatt', operation)
    assertDesktopOrderedReceiptVersion(this.central, operation)
    const startHandle = reserve.call(this.central)
    if (!isCanonicalIngressOrdinal(startHandle)) throw contractError('protocol.malformed', 'boundary', operation)
    let request: { readonly id: string; readonly count: number } | null = null
    const remember = async (
      result: import('../../core/ordered-recording-start').OrderedRecordingStartResult,
      id: string
    ) => {
      const session = this.orderedRecorderSessions.get(id)
      if (result.kind === 'prepare-failed') {
        if (session?.startHandle === startHandle) this.orderedRecorderSessions.delete(id)
        return result
      }
      if (!session || session.startHandle !== startHandle) throw contractError('ownership.denied', 'gatt', operation)
      if (result.kind === 'started' && result.receipt.state === 'active' && !session.finished) {
        if (session.recovery === null) {
          const raw = parse(await issueRecovery.call(this.central, startHandle))
          if (typeof raw !== 'object' || raw === null || Array.isArray(raw))
            throw contractError('protocol.malformed', 'boundary', operation)
          const capability: unknown = Reflect.get(raw, 'capability')
          if (
            Object.keys(raw).sort().join(',') !== 'capability,collectionId,peerId,recordingId' ||
            Reflect.get(raw, 'recordingId') !== id ||
            Reflect.get(raw, 'collectionId') !== result.receipt.collectionId ||
            Reflect.get(raw, 'peerId') !== session.peerId ||
            typeof capability !== 'string' ||
            !/^[0-9a-f]{64}$/.test(capability)
          )
            throw contractError('protocol.malformed', 'boundary', operation)
          if (!session.finished && this.orderedRecorderSessions.get(id) === session)
            session.recovery = JSON.stringify(raw)
        }
        if (!session.finished && this.orderedRecorderSessions.get(id) === session) session.active = true
      }
      return result
    }
    const parse = (value: string): unknown => {
      try {
        return JSON.parse(value)
      } catch {
        throw contractError('protocol.malformed', 'boundary', operation)
      }
    }
    return {
      start: async (
        members: Parameters<NonNullable<GattDatabase<string, string, string>['subscribeOrdered']>>[0],
        options: import('../../backend-contract/operations').RecordedSubscriptionOptions
      ) => {
        assertDesktopOrderedReceiptVersion(this.central, operation)
        const database = this.databases.get(String(base.databaseId))
        if (!database || database.nativeConnectionGeneration === null || database.nativeDatabaseGeneration === null)
          throw contractError('gatt.stale-handle', 'gatt', operation)
        const connectionGeneration = database.nativeConnectionGeneration
        const databaseGeneration = database.nativeDatabaseGeneration
        if (
          !options.recording ||
          !isRecordingId(options.recording.id) ||
          !Number.isSafeInteger(options.recording.maxBytes) ||
          options.recording.maxBytes < MIN_RECORDING_BYTES ||
          options.recording.maxBytes > MAX_RECORDING_BYTES ||
          !Number.isSafeInteger(options.recording.maxRecords) ||
          options.recording.maxRecords < MIN_RECORDING_RECORDS ||
          options.recording.maxRecords > MAX_RECORDING_RECORDS
        )
          throw contractError('argument.invalid', 'gatt', operation)
        const record = this.connectionForPath(base, operation)
        if (record.state !== 'connected' || record.coreGeneration !== database.nativeConnectionGeneration)
          throw contractError('gatt.stale-handle', 'gatt', operation)
        if (
          !members.length ||
          new Set(members.map(member => member.memberId)).size !== members.length ||
          members.some(member => !member.memberId)
        )
          throw contractError('argument.invalid', 'gatt', operation)
        const nativeMembers = members.map((member, index) => {
          const path = this.resolveCharacteristic(base, member.path, operation)
          this.assertDeliveryMode(path, member.deliveryMode, operation)
          const requirement = this.coreRequirement(member.deliveryMode)
          const preference =
            member.deliveryMode === 'prefer-notification'
              ? 'notification'
              : member.deliveryMode === 'prefer-indication'
                ? 'indication'
                : null
          return {
            memberId: String(index),
            publicMemberId: member.memberId,
            selector: this.selectorFor(path, operation),
            ...(requirement === null ? {} : { deliveryMode: requirement }),
            ...(preference === null ? {} : { preferredDeliveryMode: preference })
          }
        })
        if (request && (request.id !== options.recording.id || request.count !== members.length))
          throw contractError('argument.invalid', 'gatt', operation)
        const previous = this.orderedRecorderSessions.get(options.recording.id)
        if (previous && previous.startHandle !== startHandle) throw contractError('ownership.denied', 'gatt', operation)
        if (!previous && this.orderedRecorderSessions.size >= 256)
          throw contractError('lifecycle.invalid-state', 'cleanup', operation)
        if (!previous)
          this.orderedRecorderSessions.set(options.recording.id, {
            databaseId: String(base.databaseId),
            startHandle,
            peerId: record.nativePeerId,
            active: false,
            finished: false,
            receiptRetired: false,
            recovery: null,
            handoff: null
          })
        request = { id: options.recording.id, count: members.length }
        const text = await this.withTicket(
          String(this.mintedCorrelation()),
          options.signal,
          operation,
          ticket =>
            start.call(this.central, {
              startHandle,
              wireVersion: 5,
              metadataVersion: 2,
              recordingId: options.recording.id,
              recordingMaxBytes: options.recording.maxBytes,
              recordingMaxRecords: options.recording.maxRecords,
              peerId: record.nativePeerId,
              lease: record.lease,
              connectionGeneration,
              databaseGeneration,
              members: nativeMembers,
              maxItems: options.delivery.itemCapacity,
              maxBytes: options.delivery.byteCapacity,
              maxControls: options.delivery.reservedControlCapacity,
              ticket,
              ...this.budget(options)
            }),
          record.nativePeerId
        )
        return await remember(
          parseOrderedRecordingStartResult(parse(text), options.recording.id, members.length, 2),
          options.recording.id
        )
      },
      status: async () => {
        const raw = parse(status.call(this.central, startHandle))
        if (typeof raw !== 'object' || raw === null || Array.isArray(raw))
          throw contractError('protocol.malformed', 'boundary', operation)
        const state = Reflect.get(raw, 'state')
        const keys = Object.keys(raw)
        if ((state === 'pending' || state === 'unknown' || state === 'retired') && keys.length === 1) return { state }
        if (state === 'failed' && keys.length === 2 && Reflect.get(raw, 'receipt') === null) return { state }
        if (state === 'result' && keys.length === 2 && request)
          return {
            state,
            result: await remember(
              parseOrderedRecordingStartResult(Reflect.get(raw, 'result'), request.id, request.count, 2),
              request.id
            )
          }
        throw contractError('protocol.malformed', 'boundary', operation)
      },
      retire: async () => {
        if (request && this.orderedRecorderSessions.get(request.id)?.recovery === null) {
          const raw = parse(status.call(this.central, startHandle))
          if (
            typeof raw === 'object' &&
            raw !== null &&
            !Array.isArray(raw) &&
            Reflect.get(raw, 'state') === 'result'
          ) {
            await remember(
              parseOrderedRecordingStartResult(Reflect.get(raw, 'result'), request.id, request.count, 2),
              request.id
            )
          }
        }
        await retire.call(this.central, startHandle)
        if (request) {
          const session = this.orderedRecorderSessions.get(request.id)
          if (session?.startHandle === startHandle) {
            session.receiptRetired = true
            if (session.finished) this.orderedRecorderSessions.delete(request.id)
          }
        }
      },
      ...(this.central.orderedRecordingReclaim &&
      this.central.confirmOrderedRecordingReclaim &&
      this.central.orderedRecordingReclaimSupported?.call(this.central) === true &&
      this.central.orderedRecordingReclaimWireVersion?.call(this.central) === 2
        ? {
            recoveryHandle: (): OrderedRecordingRecoveryHandle => {
              const session = request && this.orderedRecorderSessions.get(request.id)
              if (!request || !session || session.startHandle !== startHandle || !session.active || !session.recovery)
                throw contractError('ownership.denied', 'gatt', operation)
              if (session.handoff === null) {
                session.handoff = Object.freeze({ kind: 'ordered-recording-recovery' })
                this.orderedRecoveryHandles.set(session.handoff, { recordingId: request.id, startHandle, busy: false })
              }
              return session.handoff
            }
          }
        : {}),
      ...(this.central.orderedMarkerWireVersion?.call(this.central) === 2 &&
      this.central.orderedMarkerWriteSupported?.call(this.central) === true &&
      this.central.reserveRecordingMarkerWrite &&
      this.central.startRecordingMarkerWrite &&
      this.central.recordingMarkerWriteStatus &&
      this.central.cancelRecordingMarkerWrite &&
      this.central.forgetRecordingMarkerWrite
        ? {
            reserveMarkerWrite: () => {
              if (!request) throw contractError('lifecycle.invalid-state', 'gatt', operation)
              const recordingId = request.id
              return this.reserveOrderedRecordingMarker(base, recordingId)
            }
          }
        : {})
    }
  }

  private async subscribeOrdered(
    base: DatabasePath<string, string, string>,
    members: readonly {
      readonly memberId: string
      readonly path: CharacteristicPath<string, string, string, string, string, 'current'>
      readonly deliveryMode: NonNullable<SubscriptionOptions['deliveryMode']>
    }[],
    options: SubscriptionOptions
  ): Promise<import('../../core/ordered-notification-contract').OrderedNotificationSetup> {
    const operation = this.op('gatt.subscribe-ordered')
    this.assertOperational(operation)
    if (options.signal?.aborted) throw contractError('operation.aborted', 'gatt', operation)
    if (options.deadline !== null && options.deadline !== undefined && options.deadline <= this.now())
      throw contractError('operation.timed-out', 'gatt', operation)
    const version = this.central.orderedGroupWireVersion
    const reserve = this.central.reserveOrderedGroup
    const open = this.central.openOrderedGroup
    const status = this.central.orderedGroupStatus
    if (
      version === undefined ||
      reserve === undefined ||
      open === undefined ||
      status === undefined ||
      this.central.pollOrderedGroup === undefined ||
      this.central.acknowledgeOrderedDelivery === undefined ||
      this.central.discardOrderedDelivery === undefined ||
      this.central.pollOrderedDiscardReceipt === undefined ||
      this.central.acknowledgeOrderedDiscardReceipt === undefined ||
      this.central.closeOrderedGroup === undefined ||
      this.central.acknowledgeOrderedGroup === undefined ||
      this.central.forgetOrderedGroup === undefined ||
      version.call(this.central) !== 5 ||
      this.central.orderedMetadataVersion?.call(this.central) !== 2
    )
      throw contractError('protocol.incompatible', 'boundary', operation)
    if (
      members.length === 0 ||
      new Set(members.map(member => member.memberId)).size !== members.length ||
      members.some(member => member.memberId.length === 0)
    )
      throw contractError('argument.invalid', 'gatt', operation)
    const database = this.databases.get(String(base.databaseId))
    if (
      database === undefined ||
      database.nativeConnectionGeneration === null ||
      database.nativeDatabaseGeneration === null
    )
      throw contractError('protocol.incompatible', 'boundary', `${operation}.discovery-generation`)
    const nativeConnectionGeneration = database.nativeConnectionGeneration
    const nativeDatabaseGeneration = database.nativeDatabaseGeneration
    const record = this.connectionForPath(base, operation)
    if (record.state !== 'connected' || record.coreGeneration !== database.nativeConnectionGeneration)
      throw contractError('gatt.stale-handle', 'gatt', operation)
    const nativeMembers = members.map((member, index) => {
      const path = this.resolveCharacteristic(base, member.path, operation)
      this.assertDeliveryMode(path, member.deliveryMode, operation)
      const requirement = this.coreRequirement(member.deliveryMode)
      const preference =
        member.deliveryMode === 'prefer-notification'
          ? 'notification'
          : member.deliveryMode === 'prefer-indication'
            ? 'indication'
            : null
      return {
        memberId: String(index),
        selector: this.selectorFor(path, operation),
        ...(requirement === null ? {} : { deliveryMode: requirement }),
        ...(preference === null ? {} : { preferredDeliveryMode: preference })
      }
    })
    let collectionId: string
    try {
      collectionId = reserve.call(this.central, record.nativePeerId)
    } catch (error) {
      throw desktopRustCoreError(error, `${operation}.reserve`)
    }
    if (!isCanonicalIngressOrdinal(collectionId))
      throw contractError('protocol.malformed', 'boundary', `${operation}.collection`)
    const stream = new DesktopOrderedStream(
      options.delivery,
      this.central,
      record.nativePeerId,
      collectionId,
      String(base.connectionGeneration),
      String(base.databaseGeneration),
      members.map(member => member.memberId)
    )
    const close = async () => {
      const cleanup = await stream.close()
      if (cleanup.group.state === 'released' && cleanup.members.every(member => member.cleanup.state === 'released'))
        record.acquiredClosers.delete(closeForConnection)
      return cleanup
    }
    const discard = async () => {
      const cleanup = await stream.discard()
      if (cleanup.group.state === 'released' && cleanup.members.every(member => member.cleanup.state === 'released'))
        record.acquiredClosers.delete(closeForConnection)
      return cleanup
    }
    const closeForConnection = async (): Promise<CleanupRecord> => (await close()).group
    record.acquiredClosers.add(closeForConnection)
    const markerSupported =
      this.central.orderedMarkerWireVersion?.call(this.central) === 2 &&
      this.central.orderedMarkerWriteSupported?.call(this.central) === true &&
      this.central.reserveOrderedMarkerWrite !== undefined &&
      this.central.startOrderedMarkerWrite !== undefined &&
      this.central.orderedMarkerWriteStatus !== undefined &&
      this.central.cancelOrderedMarkerWrite !== undefined &&
      this.central.forgetOrderedMarkerWrite !== undefined
    const reserveMarker = markerSupported
      ? desktopMarkerReservations(
          this.central,
          { wireVersion: 2, peerId: record.nativePeerId, collectionId },
          (target: CharacteristicPath<string, string, string, string, string, 'current'>) =>
            this.selectorFor(this.resolveCharacteristic(base, target, operation), operation),
          () => this.now(),
          () => String(this.mintedCorrelation())
        )
      : null
    const publicStream: import('../../core/ordered-notification-contract').OrderedNotificationStream = {
      ...(reserveMarker ? { reserveMarkerWrite: reserveMarker } : {}),
      limits: stream.limits,
      close,
      discard,
      [Symbol.asyncIterator]: () => {
        const iterator = stream[Symbol.asyncIterator]()
        return {
          next: () => iterator.next(),
          return: async () => {
            await discard()
            return { done: true, value: undefined }
          }
        }
      }
    }
    try {
      await this.withTicket(
        String(this.mintedCorrelation()),
        options.signal,
        operation,
        ticket =>
          open.call(this.central, {
            wireVersion: 5,
            metadataVersion: 2,
            peerId: record.nativePeerId,
            collectionId,
            lease: record.lease,
            connectionGeneration: nativeConnectionGeneration,
            databaseGeneration: nativeDatabaseGeneration,
            members: nativeMembers,
            maxItems: options.delivery.itemCapacity,
            maxBytes: options.delivery.byteCapacity,
            maxControls: options.delivery.reservedControlCapacity,
            ticket,
            ...this.budget(options)
          }),
        record.nativePeerId
      )
      const inventory = parseOrderedStatus(
        await status.call(this.central, { peerId: record.nativePeerId, collectionId }),
        members.map(member => member.memberId)
      )
      if (options.signal?.aborted) throw contractError('operation.aborted', 'gatt', operation)
      if (options.deadline !== null && options.deadline !== undefined && options.deadline <= this.now())
        throw contractError('operation.timed-out', 'gatt', operation)
      if (inventory.members.some(member => member.acquisition !== 'acquired'))
        throw contractError('protocol.malformed', 'boundary', `${operation}.inventory`)
      stream.setSourceGeneration(inventory.sourceGeneration)
      return {
        state: 'ready',
        sourceGeneration: inventory.sourceGeneration,
        members: members.map((member, index) => {
          const memberStatus = inventory.members[index]
          if (memberStatus === undefined)
            throw contractError('protocol.malformed', 'boundary', `${operation}.inventory`)
          return {
            memberId: member.memberId,
            path: member.path,
            requestedDelivery: member.deliveryMode,
            effectiveDelivery: memberStatus.effectiveDelivery
          }
        }),
        stream: publicStream
      }
    } catch (error) {
      let primary = desktopRustCoreError(error, operation).normalized
      const failures: import('../../backend-contract/errors').CleanupFailure[] = []
      let sourceGeneration: import('../../core/ordered-notification-contract').OrderedSourceGeneration | null = null
      let acquisitions: readonly {
        readonly memberId: string
        readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
      }[]
      try {
        const inventory = parseOrderedStatus(
          await status.call(this.central, { peerId: record.nativePeerId, collectionId }),
          members.map(member => member.memberId)
        )
        sourceGeneration = inventory.sourceGeneration
        if (inventory.setupError !== null) primary = inventory.setupError
        stream.setSourceGeneration(sourceGeneration)
        acquisitions = inventory.members.map(member => ({ memberId: member.memberId, acquisition: member.acquisition }))
      } catch (statusError) {
        failures.push({
          resourceKind: 'ordered-status',
          error: desktopRustCoreError(statusError, `${operation}.status`).normalized
        })
        acquisitions = members.map(member => ({ memberId: member.memberId, acquisition: 'indeterminate' }))
      }
      let cleanup: import('../../core/ordered-notification-contract').OrderedNotificationCleanup
      try {
        cleanup = await close()
      } catch (cleanupError) {
        const normalized = desktopRustCoreError(cleanupError, `${operation}.cleanup`).normalized
        const pending: CleanupRecord = {
          state: 'release-failed',
          failures: [{ resourceKind: 'ordered-group', error: normalized }]
        }
        cleanup = { group: pending, members: members.map(member => ({ memberId: member.memberId, cleanup: pending })) }
      }
      let observations: readonly (
        | import('../../core/ordered-notification-contract').OrderedNotificationValue
        | import('../../core/ordered-notification-contract').OrderedNotificationLoss
        | import('../../core/ordered-notification-contract').OrderedNotificationMarker
      )[]
      try {
        const drained = await stream.setupObservations()
        observations = drained.observations
        if (drained.error !== null) failures.push({ resourceKind: 'ordered-observations', error: drained.error })
      } catch (observationError) {
        failures.push({
          resourceKind: 'ordered-observations',
          error: desktopRustCoreError(observationError, `${operation}.observations`).normalized
        })
        observations = [
          {
            kind: 'loss',
            scope: { kind: 'unknown' },
            reason: 'control-loss',
            lostValues: null,
            lostBytes: null,
            lostSourceRecords: null
          }
        ]
      }
      if (failures.length > 0)
        cleanup = { ...cleanup, group: { state: 'release-failed', failures: [...cleanup.group.failures, ...failures] } }
      return {
        state: 'failed',
        sourceGeneration,
        error: primary,
        members: acquisitions,
        observations,
        uncertainTail: true,
        cleanup,
        stream: publicStream,
        close
      }
    }
  }

  private async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    const operation = this.op('gatt.discover')
    this.assertOperational(operation)
    const record = this.connectionsById.get(String(connection.connectionId))
    // Never address the core with an empty lease: without a live connection
    // lease the handle is stale and the core cannot route it.
    if (record === undefined || record.state !== 'connected') {
      throw contractError('gatt.stale-handle', 'gatt', `${operation}.lease`)
    }
    const correlation = String(this.mintedCorrelation())
    // The core registers the whole snapshot or rejects with a typed error
    // (a malformed platform UUID, a database past the ATT handle space);
    // a partial snapshot never becomes current.
    const discovery = await this.withTicket(
      correlation,
      options.signal,
      operation,
      async ticket => {
        const report = await this.central.discover({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(options)
        })
        return { report, paths: await this.central.discoveredPaths(record.nativePeerId) }
      },
      record.nativePeerId
    )
    const tree = groupCorePaths(discovery.paths, `${operation}.shape`)
    const names = LEGACY_DESKTOP_NAMES[this.profile.platform]
    const databaseOrdinal = names.databasePerConnection ? record.nextDatabase : this.nextDatabase
    if (names.databasePerConnection) record.nextDatabase += 1
    else this.nextDatabase += 1
    const scope = `${this.profile.platform}-rust-core`
    const databaseId = this.identifiers.databaseId(`${this.profile.platform}-database-${databaseOrdinal}`)
    const base: DatabasePath<string, string, string> = Object.freeze({
      ...record.path,
      databaseId,
      databaseGeneration: opaqueId(names.databaseGeneration(databaseOrdinal), 'database-generation', scope)
    })
    const services = tree.map(service => ({
      service,
      serviceOccurrence: this.mintOccurrence('service-occurrence', service.occurrence),
      characteristics: service.characteristics.map(characteristic => ({
        characteristic,
        characteristicOccurrence: this.mintOccurrence('characteristic-occurrence', characteristic.occurrence),
        descriptors: characteristic.descriptors.map(descriptor =>
          this.mintOccurrence('descriptor-occurrence', descriptor.occurrence)
        )
      }))
    }))
    this.databases.set(String(databaseId), {
      base,
      services,
      connectionId: String(record.path.connectionId),
      nativeConnectionGeneration: discovery.report.connectionGeneration ?? null,
      nativeDatabaseGeneration: discovery.report.databaseGeneration ?? null
    })
    record.databases.add(String(databaseId))
    return Object.freeze({
      path: base,
      reserveOrderedRecordingStart: () => this.reserveOrderedRecordingStart(base),
      reserveOrderedRecordingMarker: (recordingId: string) => this.reserveOrderedRecordingMarker(base, recordingId),
      reserveOrderedRecordingReclaim: (handle: OrderedRecordingRecoveryHandle) =>
        this.reserveOrderedRecordingReclaim(base, handle),
      subscribeOrdered: (
        members: Parameters<NonNullable<GattDatabase<string, string, string>['subscribeOrdered']>>[0],
        orderedOptions: SubscriptionOptions
      ) => this.subscribeOrdered(base, members, orderedOptions),
      snapshot: async () => this.databaseSnapshot(base),
      read: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ): Promise<CharacteristicRead> => {
        const { value, provenance } = await this.read(
          this.resolveCharacteristic(base, characteristic, this.op('gatt.database-read')),
          {
            operation: {
              signal: readOptions.signal,
              deadline: readOptions.deadline,
              correlation: this.mintedCorrelation()
            }
          },
          'gatt.database-read'
        ).completion
        return Object.freeze({ value, provenance })
      },
      write: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        value: Uint8Array,
        writeOptions: import('../../backend-contract/operations').WritePolicy
      ): Promise<WriteReceipt<string, string>> =>
        this.write(
          this.resolveCharacteristic(base, characteristic, this.op('gatt.database-write')),
          {
            operation: {
              signal: writeOptions.signal,
              deadline: writeOptions.deadline,
              correlation: this.mintedCorrelation()
            },
            bytes: value,
            mode: writeOptions.mode
          },
          'gatt.database-write'
        ).completion,
      writeWhenReady: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        value: Uint8Array,
        writeOptions: import('../../backend-contract/operations').WritePolicy
      ): Promise<WriteReceipt<string, string>> =>
        this.write(
          this.resolveCharacteristic(base, characteristic, this.op('gatt.database-write-when-ready')),
          {
            operation: {
              signal: writeOptions.signal,
              deadline: writeOptions.deadline,
              correlation: this.mintedCorrelation()
            },
            bytes: value,
            mode: writeOptions.mode
          },
          'gatt.database-write-when-ready',
          true
        ).completion,
      acquireWrite: (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        acquireOptions: PublicOperationOptions
      ) =>
        this.acquireGattWriter(
          this.resolveCharacteristic(base, characteristic, this.op('gatt.acquire-write')),
          acquireOptions
        ),
      acquireNotifications: (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        acquireOptions: SubscriptionOptions
      ) =>
        this.acquireGattNotifications(
          this.resolveCharacteristic(base, characteristic, this.op('gatt.acquire-notify')),
          acquireOptions
        ),
      readDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        readOptions: PublicOperationOptions
      ) =>
        (
          await this.readDescriptor(
            this.resolveDescriptor(base, descriptor, this.op('gatt.database-read-descriptor')),
            {
              operation: {
                signal: readOptions.signal,
                deadline: readOptions.deadline,
                correlation: this.mintedCorrelation()
              }
            },
            'gatt.database-read-descriptor'
          ).completion
        ).value,
      writeDescriptor: async (
        descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
        value: Uint8Array,
        writeOptions: import('../../backend-contract/operations').WritePolicy
      ): Promise<WriteReceipt<string, string>> =>
        this.writeDescriptor(
          this.resolveDescriptor(base, descriptor, this.op('gatt.database-write-descriptor')),
          {
            operation: {
              signal: writeOptions.signal,
              deadline: writeOptions.deadline,
              correlation: this.mintedCorrelation()
            },
            bytes: value,
            mode: writeOptions.mode
          },
          'gatt.database-write-descriptor'
        ).completion,
      subscribe: async (
        characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
        subscribeOptions: SubscriptionOptions
      ) => this.databaseSubscribe(base, characteristic, subscribeOptions)
    })
  }

  private storedDatabase(path: { readonly databaseId: unknown }, operation: string): StoredCoreDatabase {
    const stored = this.databases.get(String(path.databaseId))
    if (stored === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    return stored
  }

  private async databaseSnapshot(
    base: DatabasePath<string, string, string>
  ): Promise<GattDatabaseSnapshot<string, string, string>> {
    const operation = this.op('gatt.snapshot')
    this.assertOperational(operation)
    const stored = this.storedDatabase(base, operation)
    const services: GattDatabaseSnapshot<string, string, string>['services'][number][] = []
    const characteristics: GattDatabaseSnapshot<string, string, string>['characteristics'][number][] = []
    const descriptors: GattDatabaseSnapshot<string, string, string>['descriptors'][number][] = []
    for (const entry of stored.services) {
      const servicePath = Object.freeze({
        ...stored.base,
        serviceUuid: uuidFromCore(entry.service.uuid, `${operation}.service`),
        serviceOccurrence: occurrenceId(entry.serviceOccurrence, 'service-occurrence')
      })
      services.push(
        Object.freeze({
          path: servicePath,
          primary: entry.service.primary,
          includedServices:
            entry.service.includedServices === null
              ? null
              : Object.freeze(
                  entry.service.includedServices.map(reference =>
                    Object.freeze({
                      uuid: uuidFromCore(reference.uuid, `${operation}.included-service`),
                      occurrence: String(reference.occurrence)
                    })
                  )
                ),
          ...(entry.service.restriction === undefined ? {} : { restriction: entry.service.restriction })
        })
      )
      for (const characteristicEntry of entry.characteristics) {
        const characteristicPath = Object.freeze({
          ...servicePath,
          characteristicUuid: uuidFromCore(characteristicEntry.characteristic.uuid, `${operation}.characteristic`),
          characteristicOccurrence: occurrenceId(
            characteristicEntry.characteristicOccurrence,
            'characteristic-occurrence'
          ),
          validity: 'current' as const
        })
        characteristics.push(
          Object.freeze({
            path: characteristicPath,
            properties: characteristicPropertiesFromBits(
              characteristicEntry.characteristic.properties,
              characteristicEntry.characteristic.access
            ),
            access: accessRequirements(
              characteristicEntry.characteristic.properties,
              characteristicEntry.characteristic.access
            )
          })
        )
        characteristicEntry.characteristic.descriptors.forEach((descriptor, index) => {
          descriptors.push(
            Object.freeze({
              path: Object.freeze({
                ...characteristicPath,
                descriptorUuid: uuidFromCore(descriptor.uuid, `${operation}.descriptor`),
                descriptorOccurrence: occurrenceId(
                  characteristicEntry.descriptors[index] ?? '',
                  'descriptor-occurrence'
                )
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

  private findCharacteristic(
    stored: StoredCoreDatabase,
    path: {
      readonly serviceUuid: unknown
      readonly serviceOccurrence: unknown
      readonly characteristicUuid: unknown
      readonly characteristicOccurrence: unknown
    },
    operation: string
  ): { service: StoredCoreService; characteristic: StoredCoreCharacteristic } {
    for (const service of stored.services) {
      if (service.service.uuid !== String(path.serviceUuid)) continue
      if (service.serviceOccurrence !== String(path.serviceOccurrence)) continue
      for (const characteristic of service.characteristics) {
        if (characteristic.characteristic.uuid !== String(path.characteristicUuid)) continue
        if (characteristic.characteristicOccurrence !== String(path.characteristicOccurrence)) continue
        return { service, characteristic }
      }
    }
    throw contractError('gatt.not-found', 'gatt', operation)
  }

  private resolveCharacteristic(
    base: DatabasePath<string, string, string>,
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): CharacteristicPath<string, string, string, string, string, 'current'> {
    const stored = this.storedDatabase(base, operation)
    const found = this.findCharacteristic(stored, characteristic, operation)
    return Object.freeze({
      ...stored.base,
      serviceUuid: uuidFromCore(found.service.service.uuid, `${operation}.service`),
      serviceOccurrence: occurrenceId(found.service.serviceOccurrence, 'service-occurrence'),
      characteristicUuid: uuidFromCore(found.characteristic.characteristic.uuid, `${operation}.characteristic`),
      characteristicOccurrence: occurrenceId(
        found.characteristic.characteristicOccurrence,
        'characteristic-occurrence'
      ),
      validity: 'current' as const
    })
  }

  private resolveDescriptor(
    base: DatabasePath<string, string, string>,
    descriptor: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): DescriptorPath<string, string, string, string, string, string, 'current'> {
    return Object.freeze({
      ...this.resolveCharacteristic(base, descriptor, operation),
      descriptorUuid: descriptor.descriptorUuid,
      descriptorOccurrence: descriptor.descriptorOccurrence
    })
  }

  private async databaseSubscribe(
    base: DatabasePath<string, string, string>,
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: SubscriptionOptions
  ): Promise<import('../../backend-contract/gatt').Subscription<string, string, string, string, string, string>> {
    const resolved = this.resolveCharacteristic(base, characteristic, this.op('gatt.database-subscribe'))
    const subscription = await this.subscribe(
      resolved,
      {
        operation: {
          signal: options.signal,
          deadline: options.deadline,
          correlation: this.mintedCorrelation()
        },
        options
      },
      'gatt.database-subscribe'
    ).completion
    return Object.freeze({
      subscriptionId: subscription.subscriptionId,
      path: subscription.path,
      values: subscription.notifications,
      remove: async (): Promise<CleanupRecord> => {
        await this.unsubscribe(subscription, {
          signal: null,
          deadline: null,
          correlation: this.mintedCorrelation()
        }).completion
        return Object.freeze({ state: 'released', failures: Object.freeze([]) })
      }
    })
  }

  private read(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.read'
  ): BackendOperationDispatch<string, CharacteristicReadResult<string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    const selector = this.selectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    const correlation = String(request.operation.correlation)
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.read')
    const completion = (async (): Promise<CharacteristicReadResult<string, string>> => {
      const raw = await this.withTicket(
        correlation,
        request.operation.signal,
        operation,
        ticket =>
          this.central.read({
            peerId: record.nativePeerId,
            lease: record.lease,
            selector,
            ticket,
            ...this.budget(request.operation)
          }),
        record.nativePeerId
      )
      return Object.freeze({
        value: ownedBytes(bytesFromCore(raw.value, operation)),
        provenance: readProvenanceFromCore(raw.provenance, operation),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }

  /**
   * A with-response write resolves once the peer acknowledged it
   * (`confirmed`). A without-response write resolves once the OS accepted
   * it for transmission: nothing confirms delivery, so its commit state is
   * `unknown` (PR210-31), never `confirmed`.
   */
  private async acquireGattNative(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions,
    kind: 'write' | 'notify',
    beforeClose: () => void = () => undefined
  ) {
    const operation = this.op(kind === 'write' ? 'gatt.acquire-write' : 'gatt.acquire-notify')
    this.assertOperational(operation)
    if (this.profile.platform !== 'bluez') throw contractError('capability.unsupported', 'gatt', operation)
    const record = this.connectionForPath(path, operation)
    const selector = this.selectorFor(path, operation)
    const acquired = await this.withTicket(
      this.mintedCorrelation(),
      options.signal,
      operation,
      ticket =>
        this.central.acquireGatt(
          { peerId: record.nativePeerId, lease: record.lease, selector, ticket, ...this.budget(options) },
          kind
        ),
      record.nativePeerId
    )
    if (typeof acquired.handle !== 'string' || acquired.handle.length === 0) {
      throw contractError('protocol.violation', 'gatt', operation)
    }
    // A native handle transfers ownership before its remaining receipt is
    // validated. Failed compensation stays on this exact connection's ledger.
    const close = this.ownedAcquiredCloser(record, acquired.handle, beforeClose)
    if (!Number.isInteger(acquired.mtu) || acquired.mtu < 23 || acquired.mtu > 517) {
      const primary = contractError('protocol.violation', 'gatt', operation)
      const cleanup = await close()
      if (cleanup.failures.length > 0) {
        throw new AggregateError(
          [primary, ...cleanup.failures.map(failure => new BackendContractError(failure.error))],
          'Invalid acquired GATT receipt; cleanup remains owned'
        )
      }
      throw primary
    }
    await this.acceptAcquiredConnection(record, close)
    return { acquired, record, close }
  }

  private ownedAcquiredCloser(
    record: ConnectionRecord,
    handle: string,
    beforeClose: () => void = () => undefined
  ): () => Promise<CleanupRecord> {
    const nativeClose = this.acquiredCloser(handle, record.lease, beforeClose)
    const close = async (): Promise<CleanupRecord> => {
      const cleanup = await nativeClose()
      if (cleanup.state === 'released') record.acquiredClosers.delete(close)
      return cleanup
    }
    record.acquiredClosers.add(close)
    return close
  }

  private async acceptAcquiredConnection(record: ConnectionRecord, close: () => Promise<CleanupRecord>): Promise<void> {
    if (record.state === 'connected') return
    const cleanup = await close()
    const primary = contractError('connection.stale', 'connection', this.op('gatt.acquire'))
    if (cleanup.failures.length > 0) {
      throw new AggregateError(
        [primary, ...cleanup.failures.map(failure => new BackendContractError(failure.error))],
        'Connection ended during acquired GATT admission; cleanup remains owned'
      )
    }
    throw primary
  }

  private acquiredCloser(
    handle: string,
    lease: string,
    beforeClose: () => void = () => undefined
  ): () => Promise<CleanupRecord> {
    let released = false
    let attempt: Promise<CleanupRecord> | null = null
    return () => {
      if (released) return Promise.resolve(Object.freeze({ state: 'released', failures: Object.freeze([]) }))
      if (attempt !== null) return attempt
      beforeClose()
      attempt = this.withTicket(this.mintedCorrelation(), null, this.op('gatt.acquired-close'), ticket =>
        this.central.closeAcquired({ handle, lease, ticket })
      )
        .then((): CleanupRecord => {
          released = true
          return Object.freeze({ state: 'released', failures: Object.freeze([]) })
        })
        .catch(
          (error): CleanupRecord =>
            Object.freeze({
              state: 'release-failed',
              failures: Object.freeze([
                {
                  resourceKind: 'acquired-gatt',
                  error: desktopRustCoreError(error, this.op('gatt.acquired-close')).normalized
                }
              ])
            })
        )
        .finally(() => {
          attempt = null
        })
      return attempt
    }
  }

  private async acquireGattWriter(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<AcquiredGattWriter<string>> {
    const { acquired, record, close } = await this.acquireGattNative(path, options, 'write')
    return Object.freeze({
      mtuBytes: acquired.mtu,
      write: async (
        value: Readonly<Uint8Array>,
        writeOptions: PublicOperationOptions
      ): Promise<WriteReceipt<string, string>> => {
        const operation = this.op('gatt.acquired-write')
        const owned = bytesToCore(value, operation)
        const correlation = this.mintedCorrelation()
        await this.withTicket(
          correlation,
          writeOptions.signal,
          operation,
          ticket =>
            this.central.acquiredWrite({
              peerId: record.nativePeerId,
              lease: record.lease,
              handle: acquired.handle,
              value: owned,
              ticket,
              ...this.budget(writeOptions)
            }),
          record.nativePeerId
        )
        return Object.freeze({ terminal: this.succeededTerminal(correlation), commitState: 'unknown' })
      },
      close
    })
  }

  private async acquireGattNotifications(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: SubscriptionOptions
  ): Promise<AcquiredGattNotifications> {
    const stream = new CoreBoundedStream<NotificationValue>(options.delivery, options.delivery.overflowPolicy)
    const stop = new AbortController()
    let closing = false
    const { acquired, record, close } = await this.acquireGattNative(path, options, 'notify', () => {
      closing = true
      stream.closeWithReason('owner-released')
      stop.abort()
    })
    const pump = async (): Promise<void> => {
      try {
        while (!closing) {
          const value = await this.withTicket(
            this.mintedCorrelation(),
            stop.signal,
            this.op('gatt.acquired-receive'),
            ticket => this.central.acquiredReceive({ handle: acquired.handle, lease: record.lease, ticket })
          )
          if (closing) return
          const bytes = ownedBytes(bytesFromCore(value, this.op('gatt.acquired-receive')))
          if (
            stream.emit(
              notificationValue(bytes, 'unknown'),
              bytes.byteLength + NOTIFICATION_BYTES,
              null,
              bytes.byteLength
            ).terminated
          )
            break
        }
      } catch (error) {
        if (!closing)
          stream.closeWithReason(
            'source-failed',
            desktopRustCoreError(error, this.op('gatt.acquired-receive')).normalized
          )
      }
      const cleanup = await close()
      if (cleanup.state === 'release-failed')
        this.noteBackgroundFailure(
          'acquired-notification-close-failed',
          cleanup.failures[0] === undefined
            ? contractError('lifecycle.invariant-violation', 'cleanup', this.op('gatt.acquired-close'), {
                domain: 'desktop-rust-core',
                code: 'missing-cleanup-failure',
                safeMessage: 'Failed acquired transport cleanup omitted its failure detail',
                metadata: {}
              })
            : new BackendContractError(cleanup.failures[0].error)
        )
    }
    const values: BoundedAsyncStream<NotificationValue> = Object.freeze({
      limits: stream.limits,
      overflowPolicy: stream.overflowPolicy,
      close,
      [Symbol.asyncIterator]: () => {
        const iterator = stream[Symbol.asyncIterator]()
        return {
          next: () => iterator.next(),
          return: async () => {
            const cleanup = await close()
            if (cleanup.state === 'release-failed')
              throw cleanup.failures[0] === undefined
                ? contractError('lifecycle.invariant-violation', 'cleanup', this.op('gatt.acquired-close'), {
                    domain: 'desktop-rust-core',
                    code: 'missing-cleanup-failure',
                    safeMessage: 'Failed acquired transport cleanup omitted its failure detail',
                    metadata: {}
                  })
                : new BackendContractError(cleanup.failures[0].error)
            return iterator.return()
          },
          [Symbol.asyncIterator]() {
            return this
          }
        }
      }
    })
    pump().catch(error => this.noteBackgroundFailure('acquired-notification-pump-failed', error))
    return Object.freeze({ mtuBytes: acquired.mtu, values, close })
  }

  private write(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.write',
    waitReady = false
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    if (waitReady && request.mode !== 'without-response') {
      throw contractError('argument.invalid', 'gatt', operation)
    }
    const selector = this.selectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    const correlation = String(request.operation.correlation)
    const value = bytesToCore(request.bytes, operation)
    const mode = request.mode
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.write')
    const completion = (async (): Promise<WriteResult<string, string>> => {
      await this.withTicket(
        correlation,
        request.operation.signal,
        operation,
        ticket =>
          waitReady
            ? this.central.writeWhenReady({
                peerId: record.nativePeerId,
                lease: record.lease,
                selector,
                value,
                mode: 'without-response',
                ticket,
                ...this.budget(request.operation)
              })
            : this.central.write({
                peerId: record.nativePeerId,
                lease: record.lease,
                selector,
                value,
                mode,
                ticket,
                ...this.budget(request.operation)
              }),
        record.nativePeerId
      )
      return Object.freeze({
        terminal: this.succeededTerminal(request.operation.correlation),
        commitState: mode === 'without-response' ? ('unknown' as const) : ('confirmed' as const)
      })
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }

  private readDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.read-descriptor'
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    const selector = this.descriptorSelectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    const correlation = String(request.operation.correlation)
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.read-descriptor')
    const completion = (async (): Promise<ReadResult<string, string>> => {
      const raw = await this.withTicket(
        correlation,
        request.operation.signal,
        operation,
        ticket =>
          this.central.readDescriptor({
            peerId: record.nativePeerId,
            lease: record.lease,
            selector,
            ticket,
            ...this.budget(request.operation)
          }),
        record.nativePeerId
      )
      return Object.freeze({
        value: ownedBytes(bytesFromCore(raw, operation)),
        terminal: this.succeededTerminal(request.operation.correlation)
      })
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }

  private writeDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.write-descriptor'
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    const selector = this.descriptorSelectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    // The core writes descriptors with a response only
    // (`DesktopCentral::write_descriptor` takes no mode): a without-response
    // descriptor write fails closed before dispatch, never silently upgraded.
    // The refusal takes no serialization slot: nothing dispatches (F6).
    if (request.mode !== 'with-response') {
      // W-R1: WinRT keeps the legacy refusal identity (the native
      // boundary threw, wrapped as `gatt.write-failed` under the backend
      // verb). Other hosts keep the fail-closed unsupported refusal: their
      // legacy backends had no error here (CoreBluetooth ignored the mode,
      // BlueZ sent a command).
      if (this.profile.platform === 'winrt') {
        throw contractError('gatt.write-failed', 'gatt', this.op('gatt.write-descriptor'))
      }
      throw contractError('capability.unsupported', 'gatt', `${operation}.mode`)
    }
    const correlation = String(request.operation.correlation)
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.write-descriptor')
    const value = bytesToCore(request.bytes, operation)
    const completion = (async (): Promise<WriteResult<string, string>> => {
      await this.withTicket(
        correlation,
        request.operation.signal,
        operation,
        ticket =>
          this.central.writeDescriptor({
            peerId: record.nativePeerId,
            lease: record.lease,
            selector,
            value,
            ticket,
            ...this.budget(request.operation)
          }),
        record.nativePeerId
      )
      return Object.freeze({
        terminal: this.succeededTerminal(request.operation.correlation),
        commitState: 'confirmed' as const
      })
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }

  /**
   * The characteristic's delivery properties from the discovered database,
   * checked against a `require-*` mode before any dispatch (FIX-PLAN
   * decision 3: the legacy CoreBluetooth property check, now on every
   * desktop host). btleplug neither selects nor reports the CCCD mode, so
   * the requirement is enforced here from properties and the delivery the
   * core observed is reported as-is (`unknown` on btleplug).
   */
  private assertDeliveryMode(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    mode: SubscriptionOptions['deliveryMode'],
    operation: string
  ): void {
    let properties: number | null = null
    for (const databaseId of this.connectionForPath(path, operation).databases) {
      const stored = this.databases.get(databaseId)
      if (stored === undefined) continue
      try {
        properties = this.findCharacteristic(stored, path, operation).characteristic.characteristic.properties
        break
      } catch {
        // Not in this database snapshot; the next one (if any) is checked.
      }
    }
    if (properties === null) {
      throw contractError('gatt.stale-handle', 'gatt', `${operation}.database`)
    }
    const notify = (properties & PROPERTY_NOTIFY) !== 0
    const indicate = (properties & PROPERTY_INDICATE) !== 0
    if (
      (mode === 'require-notification' && !notify) ||
      (mode === 'require-indication' && !indicate) ||
      (!notify && !indicate)
    ) {
      throw contractError('gatt.property-not-supported', 'gatt', operation)
    }
  }

  /**
   * The CCCD requirement carried to the core. Every desktop platform
   * forwards a hard requirement. A preference is never a requirement:
   * it is forwarded separately so a selecting platform can honor it.
   */
  private coreRequirement(mode: SubscriptionOptions['deliveryMode']): 'notification' | 'indication' | null {
    if (mode === 'require-notification') return 'notification'
    if (mode === 'require-indication') return 'indication'
    return null
  }

  /**
   * Obtain the explicit retained-failure handle for one desktop subscription.
   * This is deliberately separate from normal notification delivery so a
   * legacy consumer cannot accidentally advance evidence it does not handle.
   */
  public peerNotificationFailureRecovery(
    subscription: BackendSubscription<string, string, string, string, string>
  ): DesktopPeerNotificationFailureRecovery {
    const stored = this.subscriptions.get(String(subscription.subscriptionId))
    if (stored === undefined) {
      throw contractError('gatt.stale-handle', 'core', this.op('gatt.notification.failure-recovery'))
    }
    const read = this.central.readPeerNotificationFailureRecovery
    const acknowledge = this.central.acknowledgePeerNotificationFailureRecovery
    if (this.central.pollNotificationV2 === undefined || read === undefined || acknowledge === undefined) {
      throw contractError('capability.unsupported', 'core', this.op('gatt.notification.failure-recovery'))
    }
    return Object.freeze({
      next: async (): Promise<DesktopPeerNotificationFailureRecoveryOffer> => {
        while (!stored.closed) {
          if (stored.recovery !== null) return stored.recovery
          // A failed durable stage deliberately retains the native head. A
          // trusted recovery consumer explicitly retries that same offer;
          // renderers cannot invoke this host-owned binding directly.
          const retry = this.central.retryPeerFailureRecoveryStaging
          if (retry !== undefined) await retry.call(this.central)
          if (stored.recovery !== null) return stored.recovery
          await new Promise<void>(resolve => {
            const wake = (): void => {
              stored.recoveryWaiters.delete(wake)
              resolve()
            }
            stored.recoveryWaiters.add(wake)
          })
        }
        throw contractError('gatt.stale-handle', 'core', this.op('gatt.notification.failure-recovery.next'))
      },
      read: async (offset: string): Promise<DesktopRustCorePeerSourceFailureRecoveryChunk> => {
        const recovery = stored.recovery
        if (recovery === null) {
          throw contractError('lifecycle.invalid-state', 'core', this.op('gatt.notification.failure-recovery.read'))
        }
        return read.call(this.central, {
          descriptor: recovery.descriptor,
          offset,
          peerId: stored.nativePeerId,
          selector: stored.selector,
          consumer: stored.consumer,
          recipientLease: recovery.descriptor.recipientLease ?? ''
        })
      },
      acknowledge: async (): Promise<void> => {
        const recovery = stored.recovery
        if (recovery === null) {
          throw contractError('lifecycle.invalid-state', 'core', this.op('gatt.notification.failure-recovery.ack'))
        }
        const lease = recovery.descriptor.recipientLease
        if (typeof lease !== 'string' || lease.length === 0) {
          throw contractError('protocol.malformed', 'core', this.op('gatt.notification.failure-recovery.ack'))
        }
        await acknowledge.call(this.central, {
          descriptor: recovery.descriptor,
          peerId: stored.nativePeerId,
          selector: stored.selector,
          consumer: stored.consumer,
          recipientLease: lease
        })
        stored.recovery = null
        for (const wake of [...stored.recoveryWaiters]) wake()
        this.wakePumps()
      }
    })
  }

  /**
   * List trusted durable recovery handles restored from the configured
   * recording ledger. Unlike `peerNotificationFailureRecovery`, these are
   * not attached to a live subscription and therefore preserve their
   * original source-time authority verbatim.
   */
  public durablePeerNotificationFailureRecoveries(): readonly DesktopDurablePeerNotificationFailureRecovery[] {
    this.assertAlive('desktop-process-host.durable-peer-failure-recovery')
    const list = this.central.listPeerNotificationFailureRecoveries
    const read = this.central.readPeerNotificationFailureRecovery
    const acknowledge = this.central.acknowledgePeerNotificationFailureRecovery
    if (list === undefined || read === undefined || acknowledge === undefined) {
      throw contractError(
        'capability.unsupported',
        'restoration',
        this.op('gatt.notification.durable-failure-recovery')
      )
    }
    return Object.freeze(
      list.call(this.central).map(handle => this.durablePeerNotificationFailureRecovery(handle, read, acknowledge))
    )
  }

  private durablePeerNotificationFailureRecovery(
    handle: DesktopRustCoreDurablePeerSourceFailureRecoveryHandle,
    read: NonNullable<DesktopRustCoreCentral['readPeerNotificationFailureRecovery']>,
    acknowledge: NonNullable<DesktopRustCoreCentral['acknowledgePeerNotificationFailureRecovery']>
  ): DesktopDurablePeerNotificationFailureRecovery {
    const parseOccurrence = (value: string): number | null => {
      if (!/^(?:0|[1-9][0-9]*)$/.test(value)) return null
      const parsed = Number(value)
      return Number.isSafeInteger(parsed) ? parsed : null
    }
    const serviceOccurrence = parseOccurrence(handle.serviceOccurrence)
    const characteristicOccurrence = parseOccurrence(handle.characteristicOccurrence)
    if (serviceOccurrence === null || characteristicOccurrence === null) {
      throw contractError('protocol.malformed', 'restoration', this.op('gatt.notification.durable-failure-recovery'))
    }
    const selector: DesktopRustCoreSelector = Object.freeze({
      serviceUuid: handle.serviceUuid,
      serviceOccurrence,
      characteristicUuid: handle.characteristicUuid,
      characteristicOccurrence
    })
    const provenance = Object.freeze({
      peerId: handle.peerId,
      selector,
      consumer: handle.consumer,
      recipientLease: handle.recipientLease,
      contiguousRead: handle.contiguousRead,
      acknowledged: handle.acknowledged,
      producerAck: handle.producerAck
    })
    return Object.freeze({
      descriptor: handle.descriptor,
      provenance,
      read: (offset: string): Promise<DesktopRustCorePeerSourceFailureRecoveryChunk> =>
        read.call(this.central, {
          descriptor: handle.descriptor,
          offset,
          peerId: handle.peerId,
          selector,
          consumer: handle.consumer,
          recipientLease: handle.recipientLease
        }),
      acknowledge: (): Promise<void> =>
        acknowledge.call(this.central, {
          descriptor: handle.descriptor,
          peerId: handle.peerId,
          selector,
          consumer: handle.consumer,
          recipientLease: handle.recipientLease
        })
    })
  }

  private subscribe(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.subscribe'
  ): BackendOperationDispatch<string, BackendSubscription<string, string, string, string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    const selector = this.selectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    this.assertDeliveryMode(path, request.options.deliveryMode, operation)
    const correlation = String(request.operation.correlation)
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.subscribe')
    const consumerOrdinal = this.nextOrdinal()
    const consumer = `${this.profile.platform}-core-consumer-${consumerOrdinal}`
    const subscriptionOrdinal = this.nextSubscription
    this.nextSubscription += 1
    const completion = (async (): Promise<BackendSubscription<string, string, string, string, string>> => {
      const requirement = this.coreRequirement(request.options.deliveryMode)
      const preference =
        request.options.deliveryMode === 'prefer-indication'
          ? 'indication'
          : request.options.deliveryMode === 'prefer-notification'
            ? 'notification'
            : null
      const enabled = await this.withTicket(
        correlation,
        request.operation.signal,
        operation,
        ticket =>
          this.central.subscribe({
            peerId: record.nativePeerId,
            lease: record.lease,
            selector,
            consumer,
            ticket,
            ...(requirement === null ? {} : { deliveryMode: requirement }),
            ...(preference === null ? {} : { preferredDeliveryMode: preference }),
            overflowPolicy: request.options.delivery.overflowPolicy,
            ...(request.terminalEvidenceScope === undefined
              ? {}
              : { terminalEvidenceScope: request.terminalEvidenceScope }),
            ...this.budget(request.operation)
          }),
        record.nativePeerId
      )
      const subscriptionId: SubscriptionId<string, string, string, string, string, string> =
        this.identifiers.subscriptionId(`${this.profile.platform}-subscription-${subscriptionOrdinal}`)
      const notifications = new CoreBoundedStream<NotificationValue>(
        request.options.delivery,
        request.options.delivery.overflowPolicy
      )
      const subscription: SubscriptionRecord = {
        nativePeerId: record.nativePeerId,
        selector,
        consumer,
        connectionId: String(record.path.connectionId),
        notifications,
        delivery: enabled.delivery,
        overflowPolicy: request.options.delivery.overflowPolicy,
        terminalEvidenceScope: request.terminalEvidenceScope ?? null,
        closed: false,
        pendingTerminal: null,
        surfacedLoss: 0,
        countersCheckedAt: -1,
        emptyPolls: 0,
        recovery: null,
        recoveryWaiters: new Set()
      }
      this.subscriptions.set(String(subscriptionId), subscription)
      record.subscriptions.add(String(subscriptionId))
      this.pumpNotifications(subscription).catch(error => this.failNotificationSource(subscription, error))
      return Object.freeze({
        subscriptionId,
        path,
        terminal: this.succeededTerminal(request.operation.correlation),
        observedDelivery: enabled.delivery,
        notifications
      })
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }

  /**
   * The scan and notification pumps poll the core every
   * `PUMP_INTERVAL_MS`; a wake (a lifecycle end, or the addon reporting new
   * work) makes every waiting pump poll immediately. Deliberately ref'd: a
   * live scan or subscription keeps the host alive.
   */
  private pumpWait(polledAt: number): Promise<void> {
    // A wake that arrived while this pump's poll was in flight already
    // announced work the poll may have missed: poll again at once.
    if (this.wakeSequence !== polledAt) return Promise.resolve()
    return new Promise<void>(resolve => {
      const done = (): void => {
        clearTimeout(timer)
        this.pumpWaiters.delete(done)
        resolve()
      }
      const timer = setTimeout(done, PUMP_INTERVAL_MS)
      this.pumpWaiters.add(done)
    })
  }

  private wakePumps(): void {
    this.wakeSequence += 1
    for (const done of [...this.pumpWaiters]) done()
  }

  /**
   * A lifecycle event ended this consumer's link or database. Native
   * callbacks delivered every value that arrived before the event ahead of
   * the stream's end, so the stream ends with `reason` only once its pump
   * has drained what the core still holds; the pump is woken to do so now.
   */
  private endSubscriptionAfterDrain(subscription: SubscriptionRecord, reason: SubscriptionLifecycleTerminal): void {
    if (subscription.closed || subscription.pendingTerminal !== null) return
    subscription.pendingTerminal = reason
    this.wakePumps()
  }

  /**
   * End a consumer's stream exactly once, after the values it already
   * accepted: a value the pump delivered before the end is still read
   * ahead of the terminal, whether or not anyone was reading when it ended.
   */
  private closeSubscription(
    subscription: SubscriptionRecord,
    reason: SubscriptionLifecycleTerminal | 'overflow' | 'closed',
    error?: NormalizedBleError,
    evidence?: TerminalEvidenceCommit
  ): void {
    if (subscription.closed) return
    subscription.closed = true
    for (const wake of [...subscription.recoveryWaiters]) wake()
    subscription.notifications.finishWithReason(reason, error ?? null, evidence)
  }

  /**
   * Values the core dropped for a lossy consumer, or lost before they reached
   * it (the radio's intake or forwarder, finding 131), reach the stream as
   * an overflow notice with the counts, as a local overflow does; delivery
   * continues. Read after a wake, and every `CORE_COUNTER_POLLS` idle polls.
   */
  private async surfaceCoreLoss(subscription: SubscriptionRecord, operation: string): Promise<void> {
    // An `error` consumer's loss ends it: the core's overflow terminal says so.
    if (subscription.overflowPolicy === 'error') return
    subscription.emptyPolls += 1
    if (subscription.countersCheckedAt === this.wakeSequence && subscription.emptyPolls % CORE_COUNTER_POLLS !== 0) {
      return
    }
    subscription.countersCheckedAt = this.wakeSequence
    let counters
    try {
      counters = await this.central.consumerCounters({
        peerId: subscription.nativePeerId,
        selector: subscription.selector,
        consumer: subscription.consumer
      })
    } catch (error) {
      if (subscription.closed || this.destroyed) return
      this.closeSubscription(subscription, 'source-failed', desktopRustCoreError(error, operation).normalized)
      return
    }
    if (counters === null || subscription.closed) return
    const lost = counters.droppedItems + counters.upstreamLost
    if (lost <= subscription.surfacedLoss) return
    subscription.surfacedLoss = lost
    subscription.notifications.observeSourceOverflow({
      kind: 'overflow',
      policy: subscription.overflowPolicy,
      droppedItems: resourceCount(lost),
      droppedBytes: resourceCount(counters.droppedBytes),
      replacedItems: resourceCount(counters.replacedItems)
    })
  }

  /** The notification pump itself failed: the stream ends `source-failed` rather than going quiet. */
  private failNotificationSource(subscription: SubscriptionRecord, error: unknown): void {
    this.noteBackgroundFailure('notification-pump-failed', error)
    if (subscription.closed || this.destroyed) return
    subscription.closed = true
    for (const wake of [...subscription.recoveryWaiters]) wake()
    subscription.notifications.closeWithReason(
      'source-failed',
      desktopRustCoreError(error, this.op('gatt.notification')).normalized
    )
  }

  private async pumpNotifications(subscription: SubscriptionRecord): Promise<void> {
    const operation = this.op('gatt.notification')
    while (!subscription.closed && !this.destroyed) {
      const polledAt = this.wakeSequence
      let poll
      try {
        const options = {
          peerId: subscription.nativePeerId,
          selector: subscription.selector,
          consumer: subscription.consumer
        }
        poll =
          this.central.pollNotificationV2 === undefined
            ? await this.central.pollNotification(options)
            : await this.central.pollNotificationV2(options)
      } catch (error) {
        if (subscription.closed || this.destroyed) return
        // A consumer whose lifecycle end is already decided ends with that
        // reason; any other poll failure is a failed source.
        if (subscription.pendingTerminal !== null) this.closeSubscription(subscription, subscription.pendingTerminal)
        else this.closeSubscription(subscription, 'source-failed', desktopRustCoreError(error, operation).normalized)
        return
      }
      if (poll.kind === 'empty') {
        if (subscription.pendingTerminal !== null) {
          this.closeSubscription(subscription, subscription.pendingTerminal)
          return
        }
        await this.surfaceCoreLoss(subscription, operation)
        if (subscription.closed) return
        await this.pumpWait(polledAt)
        continue
      }
      if (poll.kind === 'value') {
        let value: OwnedBytes
        try {
          value = ownedBytes(bytesFromCore(poll.value, operation))
        } catch (error) {
          // A value the binding handed over malformed is a failed source,
          // never a dropped observation (LEGACY-AUDIT-2 N6; React Native #56).
          subscription.closed = true
          subscription.notifications.closeWithReason('source-failed', desktopRustCoreError(error, operation).normalized)
          return
        }
        const receipt = poll.receipt
        if (
          (poll.sourceGeneration == null) !== (poll.sourceOrdinal == null) ||
          (poll.sourceGeneration != null &&
            (!isCanonicalIngressOrdinal(poll.sourceGeneration) || !isCanonicalIngressOrdinal(poll.sourceOrdinal)))
        ) {
          this.closeSubscription(
            subscription,
            'source-failed',
            contractError('protocol.malformed', 'core', `${operation}.notification-source-order`).normalized
          )
          return
        }
        if (poll.provenance != null && !isNotificationProvenance(poll.provenance)) {
          this.closeSubscription(
            subscription,
            'source-failed',
            contractError('protocol.malformed', 'core', `${operation}.notification-provenance`).normalized
          )
          return
        }
        if (receipt !== undefined && receipt !== null && !isNotificationReceipt(receipt)) {
          subscription.closed = true
          subscription.notifications.closeWithReason(
            'source-failed',
            desktopRustCoreError(
              contractError('protocol.malformed', 'core', `${operation}.notification-receipt`),
              operation
            ).normalized
          )
          return
        }
        subscription.notifications.emit(
          notificationValue(
            value,
            subscription.delivery,
            receipt ?? undefined,
            poll.provenance ?? 'unknown',
            poll.sourceGeneration ?? undefined,
            poll.sourceOrdinal ?? undefined
          ),
          NOTIFICATION_BYTES
        )
        continue
      }
      if (poll.kind === 'source-failure-recovery') {
        const recovery = poll.recovery
        const recoveryError = poll.recoveryError
        const parsedRecoveryError =
          typeof recoveryError === 'string' ? parseDesktopRustCoreWireError(recoveryError) : null
        if (
          recovery === null ||
          recovery === undefined ||
          recovery.protocol !== 1 ||
          recovery.representation !== 'ubm-peer-source-failure/1' ||
          typeof recovery.recipientLease !== 'string' ||
          recovery.recipientLease.length === 0 ||
          parsedRecoveryError === null
        ) {
          this.closeSubscription(
            subscription,
            'source-failed',
            contractError('protocol.malformed', 'core', `${operation}.failure-recovery`).normalized
          )
          return
        }
        subscription.recovery = Object.freeze({ descriptor: recovery, error: parsedRecoveryError })
        for (const wake of [...subscription.recoveryWaiters]) wake()
        // Do not poll or auto-ACK while an explicit recipient owns the
        // source-time lease. `acknowledge` wakes this pump after the core
        // confirms its per-recipient commit.
        await new Promise<void>(resolve => {
          const wake = (): void => {
            subscription.recoveryWaiters.delete(wake)
            resolve()
          }
          subscription.recoveryWaiters.add(wake)
        })
        continue
      }
      if (poll.kind === 'peer-source-loss') {
        if (
          typeof poll.peerId !== 'string' ||
          !isPeerLossU64(poll.generation) ||
          (poll.skippedRecords != null && !isPeerLossU64(poll.skippedRecords))
        ) {
          this.closeSubscription(
            subscription,
            'source-failed',
            contractError('protocol.malformed', 'core', `${operation}.peer-source-loss`).normalized
          )
          return
        }
        subscription.notifications.observeSourceLoss({
          kind: 'source-loss',
          scope: 'peer',
          peerId: poll.peerId,
          generation: poll.generation,
          skippedRecords: poll.skippedRecords ?? null
        })
        if (subscription.notifications.isTerminal()) {
          this.closeSubscription(subscription, 'source-failed')
          return
        }
        continue
      }
      if (poll.kind === 'terminal' && subscription.pendingTerminal === null) {
        // The core ended an `error` consumer on overflow, its own or values
        // lost upstream: the stream ends overflow with the core's counts, as
        // a local `error` overflow does.
        const evidence = await this.commitPendingTerminalEvidence(
          poll.evidence,
          'subscription',
          subscription.terminalEvidenceScope,
          Object.freeze({
            kind: 'terminal',
            reason: 'overflow',
            droppedItems: resourceCount(poll.droppedItems ?? 0),
            droppedBytes: resourceCount(poll.droppedBytes ?? 0),
            replacedItems: resourceCount(poll.replacedItems ?? 0)
          }),
          operation
        )
        subscription.closed = true
        subscription.notifications.closeWithSourceOverflow(
          {
            kind: 'overflow',
            policy: subscription.overflowPolicy,
            droppedItems: resourceCount(poll.droppedItems ?? 0),
            droppedBytes: resourceCount(poll.droppedBytes ?? 0),
            replacedItems: resourceCount(poll.replacedItems ?? 0)
          },
          evidence
        )
        return
      }
      const evidence =
        poll.kind === 'terminal'
          ? this.terminalEvidenceFromCore(poll.evidence, 'subscription', subscription.terminalEvidenceScope, operation)
          : undefined
      this.closeSubscription(
        subscription,
        subscription.pendingTerminal ??
          (poll.kind === 'terminal'
            ? 'overflow'
            : poll.kind === 'invalidated'
              ? poll.cause === 'services-changed'
                ? 'service-changed'
                : poll.cause === 'adapter-reset'
                  ? 'source-failed'
                  : 'connection-lost'
              : 'closed'),
        undefined,
        evidence
      )
      return
    }
  }

  private unsubscribe(
    subscription: BackendSubscription<string, string, string, string, string>,
    operationOptions: OperationOptions<string, string>
  ): BackendOperationDispatch<string, OperationTerminalRecord<string, string>> {
    const operation = this.op('gatt.unsubscribe')
    this.assertOperational(operation)
    const stored = this.subscriptions.get(String(subscription.subscriptionId))
    const correlation = String(operationOptions.correlation)
    // F6: unsubscribe serializes on the subscription's connection, as the
    // legacy dispatcher keyed it by connection id.
    const gattConnection = String(subscription.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.unsubscribe')
    const completion = (async (): Promise<OperationTerminalRecord<string, string>> => {
      if (stored !== undefined) {
        stored.closed = true
        await this.withTicket(
          correlation,
          operationOptions.signal,
          operation,
          ticket =>
            this.central.unsubscribe({
              peerId: stored.nativePeerId,
              selector: stored.selector,
              consumer: stored.consumer,
              ticket,
              ...this.budget(operationOptions)
            }),
          stored.nativePeerId
        )
        this.subscriptions.delete(String(subscription.subscriptionId))
        this.connectionsById.get(stored.connectionId)?.subscriptions.delete(String(subscription.subscriptionId))
        stored.notifications.closeWithReason('owner-released')
      }
      return this.succeededTerminal(operationOptions.correlation)
    })()
    return this.dispatchFor(correlation, this.trackGattVerb(gattConnection, completion))
  }
}

/**
 * Whether two adapter snapshots agree on the observable adapter state
 * (availability, authorization, power and its reason). Generation and
 * measurement time are compared separately by the caller that owns them.
 */
function sameObservableAdapterState(
  previous: AdapterStateSnapshot<string>,
  next: AdapterStateSnapshot<string>
): boolean {
  return (
    next.availability === previous.availability &&
    next.authorization === previous.authorization &&
    next.power === previous.power &&
    next.safeReason === previous.safeReason
  )
}

function adapterPower(power: DesktopRustCoreAdapterPower | null): 'on' | 'off' | 'resetting' | 'unknown' {
  if (power === 'powered-on') return 'on'
  if (power === 'powered-off') return 'off'
  if (power === 'resetting') return 'resetting'
  return 'unknown'
}

/**
 * What each OS's legacy backend announced on adapter loss: CoreBluetooth
 * (`corebluetooth-backend.ts` terminalizeAdapterLossConnection +
 * advanceGeneration) and WinRT (`winrt-backend.ts`) ended each link with
 * `connection-state-changed` reason `adapter`; CoreBluetooth and BlueZ
 * (`bluez-backend-runtime.ts` advanceBackendGeneration) announced
 * `backend-restarted`.
 */
const ADAPTER_LOSS_SEQUENCE: Readonly<
  Record<DesktopRustCorePlatform, { readonly connectionStateChanged: boolean; readonly backendRestarted: boolean }>
> = Object.freeze({
  corebluetooth: Object.freeze({ connectionStateChanged: true, backendRestarted: true }),
  winrt: Object.freeze({ connectionStateChanged: true, backendRestarted: false }),
  bluez: Object.freeze({ connectionStateChanged: false, backendRestarted: true })
})

/** The delivery kind the owner reported on each value (FIX-PLAN decision 5). */
function notificationValue(
  value: OwnedBytes,
  delivery: 'notification' | 'indication' | 'unknown',
  receipt?: NotificationValue['receipt'],
  provenance: NotificationValue['provenance'] = 'unknown',
  sourceGeneration?: string,
  sourceOrdinal?: string
): NotificationValue {
  return Object.freeze({
    value,
    delivery,
    provenance,
    ...(sourceGeneration === undefined ? {} : { sourceGeneration, sourceOrdinal }),
    ...(receipt === undefined ? {} : { receipt: Object.freeze({ ...receipt }) })
  })
}

/** A generation string from the core, checked before it reaches the host's privileged controller. */
function pairingGenerationValue(value: string): 'legacy-only' | 'enabled' | 'required' {
  if (value === 'legacy-only' || value === 'enabled' || value === 'required') return value
  throw contractError('protocol.malformed', 'core', 'desktop-rust-core.pairing-generation')
}

/** Re-brand a stored occurrence numeral string (minted by `mintOccurrence`) for a snapshot path. */
function occurrenceId<Kind extends string>(value: string, kind: Kind): GenerationId<Kind, string> {
  return opaqueId(value, kind, 'desktop-rust-core')
}

/**
 * Group the flat dispatch discovery paths into the database tree. Service
 * paths open the service node; characteristic paths attach below their
 * service; descriptor paths attach below their characteristic. Anything else
 * fails closed: inventing tree structure the core did not report would
 * corrupt every later selector.
 */
function groupCorePaths(paths: readonly DesktopRustCorePath[], operation: string): readonly CoreServiceNode[] {
  interface MutableCharacteristic {
    readonly uuid: string
    readonly occurrence: number
    readonly properties: number
    readonly access: DesktopRustCoreCharacteristicAccess | null
    readonly descriptors: Map<string, CoreDescriptorNode>
  }
  const services = new Map<
    string,
    {
      uuid: string
      occurrence: number
      primary: boolean | null
      includedServices: readonly CoreDescriptorNode[] | null
      restriction?: ReturnType<typeof serviceAccessRestriction>
      characteristics: Map<string, MutableCharacteristic>
    }
  >()
  const key = (uuid: string, occurrence: number): string => `${uuid}#${occurrence}`
  for (const path of paths) {
    if (typeof path.serviceUuid !== 'string' || !Number.isSafeInteger(path.serviceOccurrence)) {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const serviceUuid = uuidFromCore(path.serviceUuid, `${operation}.service`)
    let service = services.get(key(serviceUuid, path.serviceOccurrence))
    if (service === undefined) {
      service = {
        uuid: serviceUuid,
        occurrence: path.serviceOccurrence,
        primary: null,
        includedServices: null,
        characteristics: new Map()
      }
      services.set(key(serviceUuid, path.serviceOccurrence), service)
    }
    if (path.characteristicUuid === undefined || path.characteristicUuid === null) {
      if (path.servicePrimary != null && typeof path.servicePrimary !== 'boolean')
        throw contractError('protocol.violation', 'gatt', `${operation}.service-primary`)
      service.primary = path.servicePrimary ?? null
      if (path.includedServices != null && !Array.isArray(path.includedServices))
        throw contractError('protocol.violation', 'gatt', `${operation}.included-services`)
      service.includedServices =
        path.includedServices == null
          ? null
          : Object.freeze(
              path.includedServices.map(reference => {
                if (typeof reference !== 'object' || reference === null || typeof reference.uuid !== 'string')
                  throw contractError('protocol.violation', 'gatt', `${operation}.included-service`)
                if (!Number.isSafeInteger(reference.occurrence) || reference.occurrence < 0)
                  throw contractError('protocol.violation', 'gatt', `${operation}.included-service-occurrence`)
                return Object.freeze({
                  uuid: String(uuidFromCore(reference.uuid, `${operation}.included-service-uuid`)),
                  occurrence: reference.occurrence
                })
              })
            )
      const restriction = serviceAccessRestriction(path.serviceAccess)
      if (restriction !== undefined) service.restriction = restriction
      continue
    }
    if (typeof path.characteristicOccurrence !== 'number' || !Number.isSafeInteger(path.characteristicOccurrence)) {
      throw contractError('protocol.malformed', 'core', operation)
    }
    if (!Number.isSafeInteger(path.properties) || path.properties < 0) {
      throw contractError('protocol.malformed', 'core', `${operation}.properties`)
    }
    const characteristicUuid = uuidFromCore(path.characteristicUuid, `${operation}.characteristic`)
    const characteristicKey = key(characteristicUuid, path.characteristicOccurrence)
    let characteristic = service.characteristics.get(characteristicKey)
    if (characteristic === undefined) {
      characteristic = {
        uuid: characteristicUuid,
        occurrence: path.characteristicOccurrence,
        properties: path.properties,
        access: path.access ?? null,
        descriptors: new Map()
      }
      service.characteristics.set(characteristicKey, characteristic)
    }
    if (path.descriptorUuid === undefined || path.descriptorUuid === null) continue
    if (typeof path.descriptorOccurrence !== 'number' || !Number.isSafeInteger(path.descriptorOccurrence)) {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const descriptorUuid = uuidFromCore(path.descriptorUuid, `${operation}.descriptor`)
    const descriptorKey = key(descriptorUuid, path.descriptorOccurrence)
    if (!characteristic.descriptors.has(descriptorKey)) {
      characteristic.descriptors.set(descriptorKey, { uuid: descriptorUuid, occurrence: path.descriptorOccurrence })
    }
  }
  return Object.freeze(
    [...services.values()].map(service =>
      Object.freeze({
        uuid: service.uuid,
        occurrence: service.occurrence,
        primary: service.primary,
        includedServices: service.includedServices,
        ...(service.restriction === undefined ? {} : { restriction: service.restriction }),
        characteristics: Object.freeze(
          [...service.characteristics.values()].map(characteristic =>
            Object.freeze({
              uuid: characteristic.uuid,
              occurrence: characteristic.occurrence,
              properties: characteristic.properties,
              access: characteristic.access,
              descriptors: Object.freeze([...characteristic.descriptors.values()])
            })
          )
        )
      })
    )
  )
}

/** Accepts in-process bytes or `{ base64 }` from out-of-process doubles. */
function bytesFromCore(value: unknown, operation: string): Uint8Array {
  if (value instanceof Uint8Array) return value
  if (Array.isArray(value)) {
    // Reject out-of-range elements loudly: Uint8Array.from would wrap them
    // modulo 256 and silently corrupt the value.
    const bytes = new Uint8Array(value.length)
    value.forEach((entry: unknown, index) => {
      if (typeof entry !== 'number' || !Number.isInteger(entry) || entry < 0 || entry > 255) {
        throw contractError('protocol.malformed', 'core', `${operation}.bytes`)
      }
      bytes[index] = entry
    })
    return bytes
  }
  if (typeof value === 'object' && value !== null) {
    const base64: unknown = Reflect.get(value, 'base64')
    if (typeof base64 === 'string') return new Uint8Array(Buffer.from(base64, 'base64'))
  }
  throw contractError('protocol.malformed', 'core', `${operation}.bytes`)
}

/** The radio's own provenance word; anything else is a malformed core answer, never a guess. */
function readProvenanceFromCore(value: unknown, operation: string): ReadProvenance {
  if (isReadProvenance(value)) return value
  throw contractError('protocol.malformed', 'core', `${operation}.provenance`)
}

function bytesToCore(value: Readonly<Uint8Array>, operation: string): Uint8Array {
  // Outbound bytes are BorrowedBytes: accept the typed array (Buffers
  // included) and reject anything else rather than coercing garbage.
  if (!(value instanceof Uint8Array)) {
    throw contractError('argument.invalid', 'gatt', `${operation}.bytes`)
  }
  return Uint8Array.from(value)
}

/** Values the core hands over are ATT attribute values: bounded well below this. */
const MAXIMUM_CORE_VALUE_BYTES = byteLimit(65535)

function ownedBytes(value: Uint8Array): OwnedBytes {
  return ownBytes(value, MAXIMUM_CORE_VALUE_BYTES)
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

function presentField<Value>(value: Value | null): AdvertisementField<Value> {
  if (value === null) {
    return Object.freeze({
      state: 'absent',
      reason: 'not reported by the dispatch surface',
      provenance: 'not-provided'
    })
  }
  return Object.freeze({ state: 'present', value, provenance: 'observed' })
}

function uuidListFromCore(value: readonly string[] | null | undefined, operation: string): readonly Uuid[] | null {
  if (value === null || value === undefined) return null
  return Object.freeze(value.map(entry => uuidFromCore(entry, operation)))
}

function absentField<Value>(reason: string): AdvertisementField<Value> {
  return Object.freeze({ state: 'absent', reason, provenance: 'not-provided' })
}

/**
 * F8: a field the radio did not provide. CoreBluetooth reports it
 * `unavailable`, as its legacy backend did; WinRT and BlueZ report it
 * `absent`. Provenance is `not-provided` on every host, as legacy.
 */
function unprovidedField<Value>(platform: DesktopRustCorePlatform, reason: string): AdvertisementField<Value> {
  if (platform === 'corebluetooth') {
    return Object.freeze({ state: 'unavailable' as const, reason, provenance: 'not-provided' as const })
  }
  return absentField<Value>(reason)
}

/**
 * Consumer-side delivery pacing between core `take` polls. This paces
 * delivery only: admission, deadlines, overflow and teardown stay
 * core-owned. The timer stays ref'd: an active scan or subscription pump is
 * outstanding work, and pumps always end on stop/unsubscribe/destroy.
 */

// Core property bits (ubm-core GATT_PROP_*): READ=0x01, WRITE=0x02,
// WRITE_NO_RESPONSE=0x04, NOTIFY=0x08, INDICATE=0x10. The remaining flags
// come from the radio's characteristic facts when it reports them; a fact
// the radio does not report is `false` with availability `unknown`, never a
// claimed `false`.
function characteristicPropertiesFromBits(
  bits: number,
  access: DesktopRustCoreCharacteristicAccess | null
): import('../../backend-contract/gatt').CharacteristicProperties {
  const fact = (value: boolean | null | undefined) => ({
    value: value === true,
    availability: typeof value === 'boolean' ? ('known' as const) : ('unknown' as const)
  })
  const broadcast = fact(access?.broadcast)
  const signedWrites = fact(access?.authenticatedSignedWrites)
  const extended = fact(access?.extendedProperties)
  const reliableWrite = fact(access?.reliableWrite)
  const auxiliaries = fact(access?.writableAuxiliaries)
  return createGattCharacteristicProperties({
    read: (bits & 0x01) !== 0,
    writeWithResponse: (bits & 0x02) !== 0,
    writeWithoutResponse: (bits & 0x04) !== 0,
    notify: (bits & PROPERTY_NOTIFY) !== 0,
    indicate: (bits & PROPERTY_INDICATE) !== 0,
    broadcast: broadcast.value,
    authenticatedSignedWrites: signedWrites.value,
    extendedProperties: extended.value,
    reliableWrite: reliableWrite.value,
    writableAuxiliaries: auxiliaries.value,
    availability: {
      broadcast: broadcast.availability,
      authenticatedSignedWrites: signedWrites.availability,
      extendedProperties: extended.availability,
      reliableWrite: reliableWrite.availability,
      writableAuxiliaries: auxiliaries.availability
    }
  })
}

/**
 * Access requirements from the radio's characteristic facts (the legacy
 * BlueZ `Flags` mapping): secure/encrypt → `encrypted`,
 * encrypt-authenticated → `authenticated`, authorize → `authorized`, a
 * plain read/write property with explicitly absent security flags → `none`;
 * incomplete facts → `unknown`.
 */
function accessRequirements(
  bits: number,
  access: DesktopRustCoreCharacteristicAccess | null
): import('../../backend-contract/gatt').GattAccessRequirements {
  if (access === null) return Object.freeze({ read: 'unknown', write: 'unknown' })
  const requirement = (
    secure: boolean | null | undefined,
    encrypt: boolean | null | undefined,
    authenticated: boolean | null | undefined,
    permitted: boolean
  ): 'none' | 'encrypted' | 'authenticated' | 'authorized' | 'unknown' => {
    if (secure === true || encrypt === true) return 'encrypted'
    if (authenticated === true) return 'authenticated'
    if (access.authorize === true) return 'authorized'
    return permitted && secure === false && encrypt === false && authenticated === false && access.authorize === false
      ? 'none'
      : 'unknown'
  }
  return Object.freeze({
    read: requirement(access.secureRead, access.encryptRead, access.encryptAuthenticatedRead, (bits & 0x01) !== 0),
    write: requirement(access.secureWrite, access.encryptWrite, access.encryptAuthenticatedWrite, (bits & 0x06) !== 0)
  })
}

/** Built-in feature ids the Rust provider can wire, when the core implements them on the OS. */
const CORE_BACKED_FEATURES = Object.freeze({
  rssi: BUILT_IN_FEATURE_IDS.connectionRssi,
  effectiveMtu: BUILT_IN_FEATURE_IDS.connectionEffectiveMtu,
  maximumWriteLength: BUILT_IN_FEATURE_IDS.maximumWriteLength,
  addressTargeting: BUILT_IN_FEATURE_IDS.peerAddressTargeting,
  security: Object.freeze([
    BUILT_IN_FEATURE_IDS.securityState,
    BUILT_IN_FEATURE_IDS.securityPair,
    BUILT_IN_FEATURE_IDS.securityCancelPairing,
    BUILT_IN_FEATURE_IDS.securityUnpair
  ])
})

/** Which core-backed capabilities this backend wires: the core must implement them on the OS. */
export interface DesktopRustCoreWiring {
  readonly connectionDirect: boolean
  readonly whenAvailable: boolean
  readonly connectionRefusals: readonly DesktopRustCoreCapabilityState[]
  readonly peerDirectory: boolean
  readonly peerKnown: boolean
  readonly peerSystemConnected: boolean
  readonly peerBonded: boolean
  readonly rssi: boolean
  readonly effectiveMtu: boolean
  readonly priority: boolean
  readonly phy: boolean
  readonly scanPlatformOptions: boolean
  readonly maximumWriteLength: boolean
  readonly addressTargeting: boolean
  readonly security: boolean
  readonly maintainConnection: boolean
  readonly pairingGeneration: boolean
  readonly writeReadiness: boolean
  readonly connectionParameters: boolean
  readonly highThroughputAcquire: boolean
}

/**
 * What the core implements on this profile's OS (the core's own capability
 * registration, read from the loaded binary), narrowed to what this
 * provider wires through to the contract.
 */
export function desktopRustCoreWiring(states: readonly DesktopRustCoreCapabilityState[]): DesktopRustCoreWiring {
  const usable = new Set(states.filter(row => row.state === 'supported' || row.state === 'limited').map(row => row.id))
  return Object.freeze({
    connectionDirect: usable.has(BUILT_IN_FEATURE_IDS.connectionDirect),
    whenAvailable: usable.has(BUILT_IN_FEATURE_IDS.connectionWhenAvailable),
    connectionRefusals: Object.freeze(
      states.filter(
        row =>
          (row.state === 'unsupported' || row.state === 'unavailable') &&
          (row.id === BUILT_IN_FEATURE_IDS.connectionDirect ||
            row.id === BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection ||
            row.id === BUILT_IN_FEATURE_IDS.connectionParameters ||
            row.id === BUILT_IN_FEATURE_IDS.connectionPriority ||
            row.id === BUILT_IN_FEATURE_IDS.connectionPhy)
      )
    ),
    peerDirectory:
      usable.has(BUILT_IN_FEATURE_IDS.peerBonded) ||
      (usable.has(BUILT_IN_FEATURE_IDS.peerKnown) && usable.has(BUILT_IN_FEATURE_IDS.peerSystemConnected)),
    peerKnown: usable.has(BUILT_IN_FEATURE_IDS.peerKnown),
    peerSystemConnected: usable.has(BUILT_IN_FEATURE_IDS.peerSystemConnected),
    peerBonded: usable.has(BUILT_IN_FEATURE_IDS.peerBonded),
    rssi: usable.has(CORE_BACKED_FEATURES.rssi),
    effectiveMtu: usable.has(CORE_BACKED_FEATURES.effectiveMtu),
    priority: usable.has(BUILT_IN_FEATURE_IDS.connectionPriority),
    phy: usable.has(BUILT_IN_FEATURE_IDS.connectionPhy),
    scanPlatformOptions: usable.has(BUILT_IN_FEATURE_IDS.scanPlatformOptions),
    maximumWriteLength: usable.has(CORE_BACKED_FEATURES.maximumWriteLength),
    addressTargeting: usable.has(CORE_BACKED_FEATURES.addressTargeting),
    security: CORE_BACKED_FEATURES.security.every(id => usable.has(id)),
    maintainConnection: usable.has(BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection),
    pairingGeneration: usable.has(BUILT_IN_FEATURE_IDS.securityPairingGeneration),
    // Windows and Linux register readiness unsupported with limitation
    // `no-readiness-signal`. macOS registers it limited when both native
    // hooks exist. A watch is offered only for that limited row.
    writeReadiness: states.some(
      row =>
        row.id === BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness &&
        (row.state === 'supported' || row.state === 'limited') &&
        row.limitation !== 'no-readiness-signal'
    ),
    connectionParameters: states.some(
      row =>
        row.id === BUILT_IN_FEATURE_IDS.connectionParameters && (row.state === 'supported' || row.state === 'limited')
    ),
    highThroughputAcquire: usable.has(BUILT_IN_FEATURE_IDS.highThroughputAcquire)
  })
}

/**
 * The capabilities this backend reports: what the Rust path executes on
 * this OS, plus the rows the legacy backend reported `unsupported` with a
 * reason (they keep that reason). A capability is never claimed where the
 * core does not implement it.
 */
export function createDesktopRustCoreFeatureRegistry(
  profile: DesktopRustCoreProfile,
  wiring: DesktopRustCoreWiring,
  maximumWriteLength: MaximumWriteLengthFeatureImplementation | null = null
): FeatureRegistry {
  // Each row binds to the TCK suite that proves it and to that suite's own
  // scenario: a catalog row to the public vertical slice, a feature row to
  // its feature scenario (the runner refuses any other binding).
  const registration = (id: (typeof BUILT_IN_FEATURE_IDS)[keyof typeof BUILT_IN_FEATURE_IDS], suiteId: string) =>
    createBackendOperationCapabilityRegistration({
      id,
      implementationVersion: DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
      sourceDigest: `${profile.platform}-rust-core-${id.replace(':', '-')}-v2`,
      tckSuiteId: suiteId,
      requiredScenarioIds: [...desktopRustCoreSuiteScenarios(suiteId)],
      operation: `${id}.invoke-without-rust-core-dispatch`
    })
  const registrations: FeatureRegistry['registrations'][number][] = []
  const refused = (
    id:
      | typeof BUILT_IN_FEATURE_IDS.connectionDirect
      | typeof BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection
      | typeof BUILT_IN_FEATURE_IDS.connectionParameters
      | typeof BUILT_IN_FEATURE_IDS.connectionPriority
      | typeof BUILT_IN_FEATURE_IDS.connectionPhy
  ) => {
    const base = registration(
      id,
      id === BUILT_IN_FEATURE_IDS.connectionParameters ||
        id === BUILT_IN_FEATURE_IDS.connectionPriority ||
        id === BUILT_IN_FEATURE_IDS.connectionPhy
        ? 'connection-controls'
        : 'capability.catalog-v2'
    )
    const refusal = wiring.connectionRefusals.find(row => row.id === id)
    const reason = refusal?.limitation ?? 'native-connection-capability-unreported'
    const state = refusal?.state === 'unavailable' ? 'unavailable' : 'unsupported'
    const limitations = Object.freeze([
      Object.freeze({
        code: reason,
        explanation: `The instantiated native radio refuses this connection mechanism: ${reason}.`,
        affectedGuarantee:
          id === BUILT_IN_FEATURE_IDS.connectionParameters
            ? 'connection-parameter observations'
            : 'connection acquisition and retention'
      })
    ])
    return Object.freeze({
      ...base,
      state,
      limitations,
      evidence: Object.freeze({
        ...base.evidence,
        receiptId: `${base.evidence.sourceDigest}:instance-blocked`,
        evidenceLevel: 'blocked' as const,
        limitations
      }),
      limits: Object.freeze({ availability: Object.freeze({ maximum: 0, minimum: null, unit: 'boolean' }) })
    })
  }
  registrations.push(
    wiring.connectionDirect
      ? registration(BUILT_IN_FEATURE_IDS.connectionDirect, 'capability.catalog-v2')
      : refused(BUILT_IN_FEATURE_IDS.connectionDirect)
  )
  registrations.push(registration(BUILT_IN_FEATURE_IDS.gattDescriptors, 'capability.catalog-v2'))
  if (wiring.highThroughputAcquire && profile.platform === 'bluez') {
    registrations.push(
      createBackendOperationCapabilityRegistration({
        id: BUILT_IN_FEATURE_IDS.highThroughputAcquire,
        implementationVersion: DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
        sourceDigest: 'bluez-rust-core-owned-acquired-gatt-v1',
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: [...desktopRustCoreSuiteScenarios('capability.catalog-v2')],
        limitations: [
          {
            code: 'optional-eligible-characteristic-acquired-fd',
            explanation:
              'AcquireWrite/AcquireNotify require eligible characteristic flags and optional native methods. The native answer supplies the MTU; conflicts and source faults remain explicit.',
            affectedGuarantee: 'availability on every characteristic'
          }
        ]
      })
    )
  }
  if (wiring.connectionRefusals.some(row => row.id === BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection)) {
    registrations.push(refused(BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection))
  }
  if (wiring.peerKnown) {
    registrations.push(
      createBackendOperationCapabilityRegistration({
        id: BUILT_IN_FEATURE_IDS.peerKnown,
        implementationVersion: DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
        sourceDigest: `${profile.platform}-rust-core-peer-known-v1`,
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: [...desktopRustCoreSuiteScenarios('capability.catalog-v2')],
        ...(profile.platform === 'corebluetooth'
          ? {
              limitations: [
                {
                  code: 'references-required',
                  explanation:
                    'Known peer retrieval resolves explicit application-scoped references; it does not enumerate every OS-known peer.',
                  affectedGuarantee: 'unfiltered OS-known enumeration'
                }
              ]
            }
          : {})
      })
    )
  }
  if (wiring.peerSystemConnected) {
    registrations.push(
      createBackendOperationCapabilityRegistration({
        id: BUILT_IN_FEATURE_IDS.peerSystemConnected,
        implementationVersion: DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
        sourceDigest: `${profile.platform}-rust-core-peer-system-connected-v1`,
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: [...desktopRustCoreSuiteScenarios('capability.catalog-v2')],
        ...(profile.platform === 'corebluetooth'
          ? {
              limitations: [
                {
                  code: 'services-required',
                  explanation:
                    'CoreBluetooth requires one or more service UUIDs for its system-connected directory. Retrieval does not acquire a connection lease.',
                  affectedGuarantee: 'unfiltered system-connected enumeration'
                }
              ]
            }
          : {})
      })
    )
  }
  if (wiring.peerBonded) registrations.push(registration(BUILT_IN_FEATURE_IDS.peerBonded, 'tck.feature.peer.bonded'))
  if (wiring.whenAvailable)
    registrations.push(
      registration(BUILT_IN_FEATURE_IDS.connectionWhenAvailable, 'tck.feature.connection.when-available')
    )
  // F7: RSSI reports integer dBm precision, as the legacy registry did.
  if (wiring.rssi) {
    registrations.push(
      Object.freeze({
        ...registration(BUILT_IN_FEATURE_IDS.connectionRssi, 'connection-controls'),
        limits: Object.freeze({
          minimumRssiIntegerPrecision: Object.freeze({ minimum: 1, maximum: 1, unit: 'dBm' })
        })
      })
    )
  }
  if (wiring.addressTargeting) {
    registrations.push(registration(BUILT_IN_FEATURE_IDS.peerAddressTargeting, 'capability.catalog-v2'))
  }
  // Every desktop OS keeps the effective-MTU route limited wherever the
  // core reports it limited. Windows reads GattSession.MaxPduSize. Linux
  // reads the BlueZ characteristic MTU. macOS returns null: CoreBluetooth
  // write length is not an ATT PDU. The row carries the contract's ATT MTU
  // bounds.
  if (wiring.effectiveMtu) {
    registrations.push(
      Object.freeze({
        ...registration(BUILT_IN_FEATURE_IDS.connectionEffectiveMtu, 'connection-controls'),
        limits: Object.freeze({
          attMtu: Object.freeze({ minimum: MINIMUM_ATT_MTU, maximum: MAXIMUM_REQUESTED_ATT_MTU, unit: 'bytes' })
        })
      })
    )
  }
  if (wiring.security) {
    for (const id of CORE_BACKED_FEATURES.security) {
      registrations.push(registration(id, `tck.feature.security.${profile.platform}`))
    }
  }
  if (wiring.maximumWriteLength && maximumWriteLength !== null) {
    registrations.push(
      Object.freeze({
        ...registration(BUILT_IN_FEATURE_IDS.maximumWriteLength, 'tck.feature.gatt.maximum-write-length'),
        implementation: maximumWriteLength,
        limits: Object.freeze({
          maximumWriteLength: Object.freeze({ minimum: 1, maximum: Number.MAX_SAFE_INTEGER, unit: 'bytes' })
        })
      })
    )
  }
  if (wiring.writeReadiness) {
    registrations.push(registration(BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness, 'connection-controls'))
  }
  if (wiring.connectionParameters) {
    registrations.push(registration(BUILT_IN_FEATURE_IDS.connectionParameters, 'connection-controls'))
  } else if (
    profile.platform === 'winrt' &&
    wiring.connectionRefusals.some(row => row.id === BUILT_IN_FEATURE_IDS.connectionParameters)
  ) {
    registrations.push(refused(BUILT_IN_FEATURE_IDS.connectionParameters))
  }
  if (wiring.phy) {
    const base = registration(BUILT_IN_FEATURE_IDS.connectionPhy, 'connection-controls')
    const limitations = Object.freeze([
      Object.freeze({
        code: 'winrt-phy-observation-only',
        explanation: 'Windows observes current TX/RX PHY; PHY requests and selection are unsupported.',
        affectedGuarantee: 'PHY request and selection'
      })
    ])
    registrations.push(
      Object.freeze({
        ...base,
        state: 'limited' as const,
        limitations,
        evidence: Object.freeze({ ...base.evidence, limitations })
      })
    )
  } else if (
    profile.platform === 'winrt' &&
    wiring.connectionRefusals.some(row => row.id === BUILT_IN_FEATURE_IDS.connectionPhy)
  ) {
    registrations.push(refused(BUILT_IN_FEATURE_IDS.connectionPhy))
  }
  if (wiring.priority) {
    registrations.push(registration(BUILT_IN_FEATURE_IDS.connectionPriority, 'connection-controls'))
  } else if (
    profile.platform === 'winrt' &&
    wiring.connectionRefusals.some(row => row.id === BUILT_IN_FEATURE_IDS.connectionPriority)
  ) {
    registrations.push(refused(BUILT_IN_FEATURE_IDS.connectionPriority))
  }
  if (wiring.scanPlatformOptions) {
    registrations.push(registration(BUILT_IN_FEATURE_IDS.scanPlatformOptions, 'capability.catalog-v2'))
  }
  if (profile.platform === 'winrt' && wiring.maintainConnection) {
    // os::windows holds GattSession.MaintainConnection(true) for every
    // connection and releases it with the connection, as the legacy addon did.
    registrations.push(registration(BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection, 'capability.catalog-v2'))
  }
  if (profile.platform === 'bluez') {
    registrations.push(...createBluezConnectionControlRegistrations(DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION))
    // Reported either way, with the privilege explanation when no host
    // controller was supplied and the adapter-wide blast radius when one
    // was (AGENTS.md: elevated privilege is permitted, never implicit).
    registrations.push(
      createBluezPairingGenerationRegistration(
        DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
        wiring.security && wiring.pairingGeneration
      )
    )
  }
  if (profile.platform === 'corebluetooth') {
    registrations.push(
      ...createCoreBluetoothUnsupportedRegistrations(DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION, {
        effectiveMtuWired: wiring.effectiveMtu
      })
    )
  }
  return createFeatureRegistry(Object.freeze(registrations))
}

/** The scenario a desktop capability row's TCK suite runs to prove it. */
function desktopRustCoreSuiteScenarios(suiteId: string): readonly string[] {
  if (suiteId === 'tck.feature.peer.bonded') return ['peer.bonded-enumeration-preserves-native-facts']
  if (suiteId === 'tck.feature.connection.when-available') return ['connection.when-available-acquires-and-releases']
  if (suiteId === 'connection-controls') return ['connection.rssi-and-att-mtu-capability-contract']
  if (suiteId === 'tck.feature.gatt.maximum-write-length') return ['gatt.maximum-write-length-boundaries']
  if (suiteId.startsWith('tck.feature.security.')) return ['security.state-pair-cancel-unpair']
  return ['scenario.scan-connect-discover-read-notify-destroy']
}

/** A plan for diagnostics and the public residual matcher, from the profile's planning context. */
export function planDesktopRustCoreScan(platform: DesktopRustCorePlatform, query: NormalizedScanQuery): ScanPlan {
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  return diagnosticServiceUuidScanPlan(
    createServiceUuidScanPlan(
      query,
      {
        backendId: profile.backendId,
        platformId: profile.platformId,
        availableObservationFields: profile.observationFields
      },
      `invalid ${platform} rust-core scan planning context`
    )
  )
}

function isPeerLossU64(value: unknown): value is string {
  return typeof value === 'string' && /^(0|[1-9][0-9]*)$/.test(value) && BigInt(value) <= 18446744073709551615n
}
