// src/backends/desktop/desktop-rust-core-provider.ts
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
import type { CleanupRecord, NormalizedBleError } from '../../backend-contract/errors'
import type { BackendAttachment, BackendAttachmentRequest, BackendEvent } from '../../backend-contract/backend'
import type {
  AdapterBackend,
  BackendConnection,
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
  type AdvertisementField,
  type AdvertisementObservation,
  type OwnerScanOptions,
  type ScanFilter,
  type SourceTimestamp
} from '../../backend-contract/advertisement'
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
import { createGattCharacteristicProperties } from '../../backend-contract/gatt'
import type {
  CharacteristicPath,
  ConnectionPath,
  DatabasePath,
  DescriptorPath,
  GattDatabase,
  GattDatabaseSnapshot,
  NotificationValue
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
  ConnectionWriteReadinessObservation,
  ConnectionWriteReadinessWatch,
  ReadRssiRequest,
  RssiMeasurement
} from '../../backend-contract/connection-controls'
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
  desktopRustCoreError,
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
  type DesktopRustCorePairOutcome,
  type DesktopRustCorePath,
  type DesktopRustCorePlatform,
  type DesktopRustCoreRadio,
  type DesktopRustCoreScanObservation,
  type DesktopRustCoreSecurityEvent,
  type DesktopRustCoreScanTerminalEvent,
  type DesktopRustCoreSecurityState,
  type DesktopRustCoreSelector,
  type DesktopRustCoreWriteReadinessEvent
} from './desktop-rust-core-binding'

export const DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

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
   * Whether a `require-*` delivery mode is carried to the core as a CCCD
   * requirement (FIX-PLAN decision 3). WinRT: yes — the Windows adapter
   * writes the CCCD itself and honours it. CoreBluetooth and BlueZ keep
   * their legacy semantics: the requirement is checked against the
   * characteristic's properties here and the platform picks the mode, so an
   * app the legacy backend accepted is never refused.
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
      deliveryRequirementToCore: false
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
      deliveryRequirementToCore: false
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
      observationFields: ADDRESSED_OBSERVATION_FIELDS,
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
   * Radio selection. `production` (default) executes the btleplug radio and
   * fails loudly with `adapter.unavailable` where no adapter exists — it
   * never falls back to synthetic. `synthetic` is the hardware-free leg and
   * must be requested explicitly (tests); the public factories never select it.
   */
  readonly radio?: DesktopRustCoreRadio
  /**
   * Injected core entry (tests). Absent, the provider loads the packaged
   * addon on first use and verifies its build identity before any radio
   * call; there is no TypeScript fallback.
   */
  readonly binding?: DesktopRustCoreBinding
  /** Resolves the packaged binding; defaults to `src/desktop-core-addon.ts`. */
  readonly loadBinding?: (profile: DesktopRustCoreProfile) => Promise<DesktopRustCoreBinding>
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
  /** Test seam for the pre-load platform guard; defaults to `process.platform`. */
  readonly hostPlatform?: string
  /** BlueZ only: the D-Bus bus (the legacy `busKind`); `system` by default. */
  readonly bluezBus?: DesktopRustCoreBluezBus
  /**
   * BlueZ only: the host's privileged pairing-generation controller. The
   * package never escalates on its own; with a controller, a pair directing
   * `secureConnections` holds the adapter generation for the ceremony and
   * the core restores it afterwards.
   */
  readonly pairingGeneration?: DesktopRustCoreGenerationController
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

/**
 * Creates the shared-core desktop provider for one platform. The platform
 * guard runs here, before anything loads; the addon itself loads lazily on
 * the first `listAdapters`/`create`, where its build identity is checked.
 */
export function createDesktopRustCoreBackendProvider(
  options: DesktopRustCoreProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  const profile = DESKTOP_RUST_CORE_PROFILES[options.platform]
  if (options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner'))
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
  if (profile.platform !== 'bluez' && (options.bluezBus !== undefined || options.pairingGeneration !== undefined)) {
    throw contractError(
      'argument.invalid',
      'core',
      desktopRustCoreOperation(profile.operationPrefix, 'bluez-only-option')
    )
  }
  const context = (resolved: DesktopRustCoreBinding): BackendOpenContext => ({
    profile,
    owner: options.owner,
    now: options.now,
    binding: resolved,
    radio,
    hostKind,
    createOwnerId,
    bluezBus: options.bluezBus ?? 'system',
    pairingGeneration: options.pairingGeneration ?? null,
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
  readonly pairingGeneration: DesktopRustCoreGenerationController | null
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
      await backend.destroy()
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
    const cleanup = await backend.destroy()
    const failure = cleanup.failures[0]
    if (cleanup.state !== 'released' && failure !== undefined) {
      throw new BackendContractError(failure.error)
    }
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
            ...(context.pairingGeneration === null ? {} : { pairingGeneration: true })
          })
        : await context.binding.openProduction({
            owner: coreOwner,
            platform: profile.platform,
            adapterId: adapter.label,
            ...(profile.platform === 'bluez' ? { bluezBus: context.bluezBus } : {}),
            ...(context.pairingGeneration === null ? {} : { pairingGeneration: true })
          })
  } catch (error) {
    throwDesktopRustCoreError(error, desktopRustCoreOperation(profile.operationPrefix, 'rust-core-open'))
  }
  const backend = new DesktopRustCoreBackend({
    profile,
    owner: coreOwner,
    central,
    now: context.now,
    radio: context.radio,
    hostKind: context.hostKind,
    adapter: adapter.descriptor,
    bindingDiagnostics: Object.freeze({
      ...(context.binding.diagnostics ?? {}),
      ...(adapter.deployment === undefined || adapter.deployment === null ? {} : { deployment: adapter.deployment })
    }),
    coreStates: context.binding.capabilityStates(profile.platform, context.pairingGeneration !== null),
    pairingGeneration: context.pairingGeneration,
    firstStateTimeoutMs: context.firstStateTimeoutMs,
    // Legacy listings opened no backend: a listing probe takes no instance number.
    backendInstance:
      purpose === 'listing' ? `${profile.platform}-backend-listing` : allocateLegacyInstance(profile.platform)
  })
  try {
    await backend.open()
    return backend
  } catch (error) {
    const cleanup = await backend.destroy().catch((destroyError: unknown) => {
      throw new AggregateError([error, destroyError], 'desktop rust core open failed and its cleanup failed')
    })
    const failure = cleanup.failures[0]
    if (cleanup.state !== 'released' && failure !== undefined) {
      throw new AggregateError(
        [error, new BackendContractError(failure.error)],
        'desktop rust core open failed and its cleanup did not release'
      )
    }
    throw error
  }
}

interface DesktopRustCoreBackendConstruction {
  readonly profile: DesktopRustCoreProfile
  readonly owner: string
  readonly central: DesktopRustCoreCentral
  readonly now: () => number
  readonly radio: DesktopRustCoreRadio
  readonly hostKind: 'node' | 'desktop-native'
  readonly adapter: AdapterDescriptor<string>
  readonly bindingDiagnostics: Readonly<Record<string, string>>
  /** The core's capability registration for this profile's OS (from the loaded binary). */
  readonly coreStates: readonly DesktopRustCoreCapabilityState[]
  readonly pairingGeneration: DesktopRustCoreGenerationController | null
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
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: readonly CoreCharacteristicNode[]
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
}

interface ConnectionRecord {
  readonly nativePeerId: string
  readonly lease: string
  readonly coreGeneration: string
  readonly path: ConnectionPath<string, string>
  state: 'connected' | 'disconnecting' | 'disconnected' | 'lost'
  readonly databases: Set<string>
  readonly subscriptions: Set<string>
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
  closed: boolean
  /** Items the core dropped or lost upstream that this stream already reported. */
  surfacedLoss: number
  /** The wake sequence at the last read of the core counters. */
  countersCheckedAt: number
  emptyPolls: number
  /**
   * The terminal a lifecycle event decided (link lost, database changed,
   * adapter lost). The stream ends with it only after the values the core
   * still holds for this consumer are delivered (LEGACY-AUDIT-4 R2).
   */
  pendingTerminal: SubscriptionLifecycleTerminal | null
}

/** A notification stream end a lifecycle event decides. */
type SubscriptionLifecycleTerminal = 'connection-lost' | 'service-changed' | 'source-failed'

interface ReadinessWatch {
  readonly record: ConnectionRecord
  readonly stream: CoreBoundedStream<ConnectionWriteReadinessObservation<string>>
  ordinal: number
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
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly peers?: PeerDirectoryBackend<string> = undefined
  /**
   * Link security through the core's OS adapters (WinRT
   * DeviceInformationPairing, BlueZ Device1.Pair + Agent1), present only
   * where the core implements it on this OS.
   */
  readonly security: SecurityBackend | undefined
  readonly features: FeatureRegistry

  private readonly profile: DesktopRustCoreProfile
  private readonly owner: string
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
  private readonly connectionsById = new Map<string, ConnectionRecord>()
  private readonly subscriptions = new Map<string, SubscriptionRecord>()
  private readonly databases = new Map<string, StoredCoreDatabase>()
  private readonly occurrenceNumerals = new Map<string, number>()
  private readonly ticketsByCorrelation = new Map<string, string>()
  private readonly adapterTransitions = new Set<CoreBoundedStream<AdapterStateSnapshot<string>>>()
  private readonly securityWatches = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly pendingAddresses = new Map<string, PeerAddressDescriptor>()
  private readonly addressTypes = new Map<string, 'public' | 'random' | null>()
  private readonly readinessWatches = new Set<ReadinessWatch>()
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
  private nextOrdinalValue = 1
  private nextEventOrdinalValue = 1
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

  constructor(construction: DesktopRustCoreBackendConstruction) {
    const { profile, owner } = construction
    this.profile = profile
    this.owner = owner
    this.central = construction.central
    this.now = construction.now
    this.radio = construction.radio
    this.hostKind = construction.hostKind
    this.bindingDiagnostics = construction.bindingDiagnostics
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
        : {})
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
    ).length
    return Object.freeze({
      activeScanControllers: resourceCount(this.scanGroup === null ? 0 : 1),
      scanConsumers: resourceCount(scanConsumers),
      chooserSessions: resourceCount(0),
      connectionLeases: resourceCount(liveConnections),
      physicalLinks: resourceCount(liveConnections),
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
    run: (ticket: string) => Promise<Result>
  ): Promise<Result> {
    if (signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', operation)
    }
    let ticket: string
    try {
      ticket = this.central.createTicket()
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

  private mintedCorrelation(kind: string): OperationCorrelation<string, string> {
    return this.identifiers.operationCorrelation(`${this.profile.platform}-core-${kind}-${this.nextOrdinal()}`)
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
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const terminal = await this.central.takeScanTerminalEvent()
        if (terminal === null || terminal === undefined) break
        if (terminal.kind === 'lagged') this.reconcileScan(terminal.missed ?? null)
        else this.applyScanTerminal(terminal)
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const adapter = await this.central.takeAdapterEvent()
        if (adapter === null || adapter === undefined) break
        if (adapter.kind === 'state' && typeof adapter.state === 'string') {
          this.applyAdapterPower(adapter.state)
        } else if (adapter.kind === 'lagged') {
          this.noteDiagnostic('adapter-events-lagged', 'adapter events were missed; re-reading adapter state', {
            missed: adapter.missed ?? null
          })
          this.applyAdapterPower(await this.central.adapterState())
        } else if (adapter.kind === 'closed') {
          this.failCoreEventSource('adapter-events-closed')
        }
      }
      for (;;) {
        if (this.destroyed || this.coreEventsClosed) return
        const reset = await this.central.takeAdapterResetEvent()
        if (reset === null || reset === undefined) break
        if (reset.kind === 'lagged') await this.reconcileAdapterReset(reset.missed ?? null)
        else this.applyAdapterReset(reset)
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
   */
  private applyAdapterLostLink(record: ConnectionRecord): void {
    if (record.state !== 'connected' && record.state !== 'disconnecting') return
    const previous = record.state
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
    if (ADAPTER_LOSS_SEQUENCE[this.profile.platform].connectionStateChanged) {
      this.emitEvent({
        kind: 'connection-state-changed',
        attachment: this.attachment,
        attachmentId: this.attachment.attachmentId,
        ingressOrdinal: this.nextEventOrdinal(),
        connection: record.path,
        previous,
        current: 'lost',
        reason: 'adapter'
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
      if (released.has(record.nativePeerId)) this.applyAdapterLostLink(record)
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
    this.advanceGeneration(ADAPTER_LOSS_SEQUENCE[this.profile.platform].backendRestarted)
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

  /** A new backend generation: new attachment, stale peer handles dropped, watchers told. */
  private advanceGeneration(announceRestart: boolean): void {
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
    for (const stream of [...this.adapterTransitions]) {
      if (stream.emit(this.adapterState, 96).terminated) this.adapterTransitions.delete(stream)
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

  private applyAdapterPower(power: DesktopRustCoreAdapterPower): void {
    if (this.destroyed) return
    // A power change keeps the last measured authorization and its reason,
    // except where the state itself decides them (unsupported).
    const snapshot = this.snapshotFor(power, null, null)
    this.adapterState =
      power === 'unsupported' || power === 'unauthorized'
        ? snapshot
        : Object.freeze({
            ...snapshot,
            authorization: this.adapterState.authorization,
            safeReason: this.adapterState.safeReason
          })
    for (const stream of [...this.adapterTransitions]) {
      if (stream.emit(this.adapterState, 96).terminated) this.adapterTransitions.delete(stream)
    }
    this.emitEvent({
      kind: 'adapter-state',
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      ingressOrdinal: this.nextEventOrdinal()
    })
  }

  private connectionFor(event: DesktopRustCoreLifecycleEvent): ConnectionRecord | null {
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId !== event.peerId) continue
      if (record.state !== 'connected' && record.state !== 'disconnecting') continue
      if (typeof event.connectionGeneration === 'string' && event.connectionGeneration !== record.coreGeneration)
        continue
      return record
    }
    return null
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
    const record = this.connectionFor(event)
    if (record === null) return
    if (event.kind === 'adapter-lost') {
      this.applyAdapterLostLink(record)
      return
    }
    if (event.kind === 'services-changed') {
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
      return
    }
    const requested = event.kind === 'released' && event.requested === true
    record.state = requested ? 'disconnected' : 'lost'
    this.invalidateConnectionState(record, 'connection-lost')
    if (requested) return
    this.emitEvent({
      kind: 'connection-lost',
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      ingressOrdinal: this.nextEventOrdinal(),
      connection: record.path
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
    }
  }

  // -- peers ---------------------------------------------------------------

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
    if (options.platform !== undefined) {
      throw contractError('capability.unsupported', 'scan', `${operation}.platform-options`)
    }
    return trustedServiceUuidFilter(options, query => this.planScan(query), operation)
  }

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    const operation = this.op('scan.start')
    this.assertOperational(operation)
    const nativeFilter = this.scanFilterFor(options, operation)
    if (this.scanGroup !== null) {
      throw contractError('scan.already-active', 'scan', operation)
    }
    const ordinal = this.nextOrdinal()
    const correlation = String(this.mintedCorrelation('scan'))
    const started = await this.withTicket(correlation, options.signal, operation, ticket =>
      this.central.startScan({
        owner: `${this.owner}/scan-${ordinal}`,
        serviceUuids: nativeFilter.serviceUuids.map(uuid => String(uuid)),
        duplicatePolicy: options.duplicatePolicy,
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
      foreignRefused: 0
    }
    this.scanGroup = group
    const consumer = this.addScanConsumer(group, ownerLeaseId, options, options.filter)
    this.pumpScanObservations(group).catch(error => this.failScanSource(group, 'scan-pump-failed', error))
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
      consumer.stream.closeWithReason('owner-released')
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
      current.stream.closeWithReason('owner-released')
    }
    group.consumers.clear()
    if (this.scanGroup === group) this.scanGroup = null
    return Object.freeze({ state: 'released', failures: Object.freeze([]) })
  }

  private async pumpScanObservations(group: ScanGroup): Promise<void> {
    while (!this.destroyed && this.scanGroup === group) {
      const polledAt = this.wakeSequence
      let taken: DesktopRustCoreScanObservation | null
      try {
        taken = await this.central.takeScanObservation()
      } catch (error) {
        await this.failScanSource(group, 'scan-take-failed', error)
        return
      }
      if (taken === null || taken === undefined) {
        await this.pumpWait(polledAt)
        continue
      }
      // 4.x reported advertisements only while a scan was active: a
      // sighting the core queued for another scan is never attributed to
      // this one (LEGACY-AUDIT-5 S2). It is refused loudly, not delivered.
      if (taken.scanOperationId !== group.coreOperationId) {
        group.foreignRefused += 1
        this.noteDiagnostic('scan-observation-foreign', 'an observation queued for another scan was refused', {
          scanOperationId: String(taken.scanOperationId).slice(0, 128),
          refused: group.foreignRefused
        })
        continue
      }
      const next = taken.advertisement
      await this.ensureAddressType(next)
      let observation: AdvertisementObservation<string>
      try {
        observation = this.mapObservation(next, group.scanSessionId, taken.ageMs)
      } catch (error) {
        this.noteBackgroundFailure('scan-record-malformed', error)
        this.countMalformedObservation(group)
        continue
      }
      this.deliverObservation(group, observation, next.peerId)
    }
  }

  /**
   * The scan source stopped (a failed take, or the pump itself failed):
   * every consumer ends `source-failed` and the owner's scan is stopped, so
   * no stream stays open on a pump that will never deliver again.
   */
  private async failScanSource(group: ScanGroup, code: string, error: unknown): Promise<void> {
    if (this.destroyed || this.scanGroup !== group) return
    this.noteBackgroundFailure(code, error)
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
      if (!advertisementMatchesFilter(consumer.filter, observation)) continue
      if (consumer.options.duplicatePolicy === 'first') {
        if (consumer.seenPeers.has(nativePeerId)) continue
        consumer.seenPeers.add(nativePeerId)
      }
      const push = consumer.stream.emit(observation, ADVERTISEMENT_BYTES)
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
                // B-R3: BlueZ maps every non-public (including unreported)
                // address type to random, as its legacy backend did; other
                // radios stay opaque rather than guess.
                type:
                  this.addressTypes.get(value.peerId) ??
                  (this.profile.platform === 'bluez' ? ('random' as const) : ('opaque' as const))
              })
            : null
      }),
      provenance,
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
    const correlation = String(this.mintedCorrelation('write-readiness'))
    const stream = new CoreBoundedStream<ConnectionWriteReadinessObservation<string>>(
      { itemCapacity: capacity(64), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
      // F1: the legacy watch dropped the oldest observation on overflow.
      'drop-oldest'
    )
    const watch: ReadinessWatch = {
      record,
      stream,
      ordinal: 0,
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
    let ready: boolean
    try {
      ready = await this.withTicket(correlation, options.signal, operation, ticket =>
        this.central.writeReadiness({
          peerId: record.nativePeerId,
          lease: record.lease,
          ticket,
          ...this.budget(options)
        })
      )
    } catch (error) {
      this.closeReadinessWatch(watch, 'source-failed', desktopRustCoreError(error, operation).normalized)
      throw error
    }
    if (!this.readinessWatches.has(watch)) {
      // The watch ended while the probe was in flight (connection lost).
      throw contractError('connection.stale', 'connection', `${operation}.closed`)
    }
    watch.probed = true
    this.emitReadiness(watch, ready)
    const pending = watch.buffered
    watch.buffered = null
    // F3: replay the latest pre-probe report for this generation, if any.
    if (pending !== null && (pending.generation === null || pending.generation === watch.record.coreGeneration)) {
      this.emitReadiness(watch, pending.ready)
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
    watch.stream.closeWithReason(reason, normalized)
  }

  private emitReadiness(watch: ReadinessWatch, ready: boolean): void {
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

  private async reprobeReadiness(watch: ReadinessWatch): Promise<void> {
    if (!this.readinessWatches.has(watch) || this.destroyed || watch.ready || !watch.probed) return
    if (watch.signal?.aborted === true) return
    if (watch.deadline !== null && this.now() > watch.deadline) return
    if (watch.record.state !== 'connected') {
      this.closeReadinessWatch(watch, 'connection-lost')
      return
    }
    let ready: boolean
    try {
      ready = await this.central.writeReadiness({ peerId: watch.record.nativePeerId, lease: watch.record.lease })
    } catch (error) {
      if (this.readinessWatches.has(watch)) {
        this.closeReadinessWatch(
          watch,
          'source-failed',
          desktopRustCoreError(error, this.op('connection.write-readiness.reconcile')).normalized
        )
      }
      return
    }
    if (this.readinessWatches.has(watch)) this.emitReadiness(watch, ready)
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
      let ready: boolean
      try {
        ready = await this.central.writeReadiness({ peerId: watch.record.nativePeerId, lease: watch.record.lease })
      } catch (error) {
        this.closeReadinessWatch(
          watch,
          'source-failed',
          desktopRustCoreError(error, this.op('connection.write-readiness.reconcile')).normalized
        )
        continue
      }
      if (this.readinessWatches.has(watch)) this.emitReadiness(watch, ready)
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
      if (typeof event.connectionGeneration === 'string' && event.connectionGeneration !== watch.record.coreGeneration)
        continue
      if (watch.record.state !== 'connected') {
        this.closeReadinessWatch(watch, 'connection-lost')
        continue
      }
      // F3: a report arriving before the probe resolves buffers (latest
      // wins) and replays after it — it is never dropped.
      if (!watch.probed) {
        watch.buffered = Object.freeze({ ready: event.ready === true, generation: event.connectionGeneration ?? null })
        continue
      }
      this.emitReadiness(watch, event.ready === true)
    }
  }

  /**
   * The OS ended the scan without a stop request (legacy WinRT
   * `OnScanTerminal`). The core already settled it and released the owner:
   * every consumer ends — `source-failed` with the OS's words when the OS
   * reported an error, `closed` otherwise — and a new scan can start.
   */
  private applyScanTerminal(event: DesktopRustCoreScanTerminalEvent): void {
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
    for (const consumer of group.consumers.values()) {
      consumer.removeAbort()
      consumer.stream.closeWithReason(error === null ? 'closed' : 'source-failed', error)
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
    // W-R3: a never-observed peer is `connection.not-found`, as every
    // legacy backend reported it — under the connect op on BlueZ, with the
    // `.peer` segment on CoreBluetooth and WinRT.
    if (pending === undefined) {
      throw contractError(
        'connection.not-found',
        'connection',
        this.profile.platform === 'bluez' ? operation : `${operation}.peer`
      )
    }
    const correlation = String(this.mintedCorrelation('resolve-address'))
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
        const correlation = String(this.mintedCorrelation('security-state'))
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
            const record: PeerSecurityEvent = Object.freeze({
              kind: 'state',
              peerId,
              sequence: this.securitySequence,
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
        const correlation = String(this.mintedCorrelation('pair'))
        const outcome = await this.withTicket(correlation, options.signal, operation('pair'), ticket =>
          this.central.pair({ peerId: nativePeerId, ticket, ...secureConnections, ...this.budget(options) })
        )
        return this.pairResult(outcome, operation('pair'))
      },
      cancelPairing: async (peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult> => {
        this.assertOperational(operation('cancel-pairing'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('cancel-pairing'))
        const correlation = String(this.mintedCorrelation('cancel-pairing'))
        const answer = await this.withTicket(correlation, options.signal, operation('cancel-pairing'), ticket =>
          this.central.cancelPairing({ peerId: nativePeerId, ticket, ...this.budget(options) })
        )
        if (answer.outcome === 'rejected') return Object.freeze({ outcome: 'rejected', reason: answer.reason ?? null })
        return Object.freeze({ outcome: answer.outcome })
      },
      unpair: async (peerId: string, options: PublicOperationOptions): Promise<SecurityUnpairResult> => {
        this.assertOperational(operation('unpair'))
        const nativePeerId = this.nativePeerForSecurity(peerId, operation('unpair'))
        const correlation = String(this.mintedCorrelation('unpair'))
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
    if (typeof event.sequence === 'number') this.securitySequence = Math.max(this.securitySequence, event.sequence)
    if (typeof event.peerId !== 'string' || event.state === null || event.state === undefined) return
    const peerId = this.peerIdsByNativeId.get(event.peerId)
    if (peerId === undefined) return
    this.deliverSecurityState(String(peerId), event.sequence ?? 0, event.state)
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
    if (options.intent !== undefined && options.intent !== 'direct') {
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
    this.assertLinkNotOwned(nativePeerId, operation)
    const ordinal = this.nextOrdinal()
    const lease = `${this.profile.platform}-core-lease-${ordinal}`
    const correlation = String(this.mintedCorrelation('connect'))
    const connected = await this.withTicket(correlation, options.signal, operation, ticket =>
      this.central.connect({ peerId: nativePeerId, lease, ticket, ...this.budget(options) })
    )
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
      nextDatabase: 1
    }
    this.connectionsById.set(String(connectionId), record)
    const release = (): Promise<CleanupRecord> => this.disconnectConnection(record)
    const connection: BackendConnection<string, string> = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId,
      connectionId,
      connectionGeneration,
      state: 'connected',
      disconnect: release
    })
    return Object.freeze({ leaseId, connection, release })
  }

  /**
   * CoreBluetooth and WinRT own one link per peer: a second connect while
   * it is live is `connection.already-owned` before any dispatch, as the
   * 4.x backends refused it. BlueZ shared the device's link between leases
   * (the dbus-next shared connection record), so it joins.
   */
  private assertLinkNotOwned(nativePeerId: string, operation: string): void {
    if (this.profile.platform === 'bluez') return
    for (const record of this.connectionsById.values()) {
      if (record.nativePeerId !== nativePeerId) continue
      if (record.state === 'connected' || record.state === 'disconnecting') {
        throw contractError('connection.already-owned', 'connection', `${operation}.owner`)
      }
    }
  }

  /**
   * Release through the core: only the OS answer releases the link. A
   * failed or timed-out release keeps the link `disconnecting` in the core
   * and reports `release-failed`; a retry drives the radio again, and a link
   * that already ended answers `already-released` without a radio call.
   */
  private async disconnectConnection(record: ConnectionRecord): Promise<CleanupRecord> {
    const operation = this.op('connection.disconnect')
    if (record.state === 'connected') record.state = 'disconnecting'
    try {
      await this.central.disconnect({ peerId: record.nativePeerId, lease: record.lease })
    } catch (error) {
      const normalized = desktopRustCoreError(error, operation).normalized
      return Object.freeze({
        state: 'release-failed',
        failures: Object.freeze([{ resourceKind: 'connection', error: normalized }])
      })
    }
    record.state = 'disconnected'
    this.invalidateConnectionState(record, 'connection-lost')
    return Object.freeze({ state: 'released', failures: Object.freeze([]) })
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
    const correlation = String(this.mintedCorrelation('discover'))
    // The core registers the whole snapshot or rejects with a typed error
    // (a malformed platform UUID, a database past the ATT handle space);
    // a partial snapshot never becomes current.
    const paths = await this.withTicket(correlation, options.signal, operation, async ticket => {
      await this.central.discover({
        peerId: record.nativePeerId,
        lease: record.lease,
        ticket,
        ...this.budget(options)
      })
      return this.central.discoveredPaths(record.nativePeerId)
    })
    const tree = groupCorePaths(paths, `${operation}.shape`)
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
    this.databases.set(String(databaseId), { base, services, connectionId: String(record.path.connectionId) })
    record.databases.add(String(databaseId))
    return Object.freeze({
      path: base,
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
              correlation: this.mintedCorrelation('gdb-read')
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
              correlation: this.mintedCorrelation('gdb-write')
            },
            bytes: value,
            mode: writeOptions.mode
          },
          'gatt.database-write'
        ).completion,
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
                correlation: this.mintedCorrelation('gdb-read-desc')
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
              correlation: this.mintedCorrelation('gdb-write-desc')
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
      services.push(Object.freeze({ path: servicePath, primary: true, includedServices: Object.freeze([]) }))
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
          correlation: this.mintedCorrelation('gdb-subscribe')
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
          correlation: this.mintedCorrelation('gdb-unsubscribe')
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
      const raw = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.read({ peerId: record.nativePeerId, selector, ticket, ...this.budget(request.operation) })
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
  private write(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>,
    /** The 4.x operation this call reports (a database handle's own id). */
    name = 'gatt.write'
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    const operation = this.op(name)
    this.assertOperational(operation)
    const selector = this.selectorFor(path, operation)
    const record = this.connectionForPath(path, `${operation}.peer`)
    const correlation = String(request.operation.correlation)
    const value = bytesToCore(request.bytes, operation)
    const mode = request.mode
    const gattConnection = String(record.path.connectionId)
    this.admitGattVerb(gattConnection, 'gatt.write')
    const completion = (async (): Promise<WriteResult<string, string>> => {
      await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.write({
          peerId: record.nativePeerId,
          selector,
          value,
          mode,
          ticket,
          ...this.budget(request.operation)
        })
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
      const raw = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.readDescriptor({
          peerId: record.nativePeerId,
          selector,
          ticket,
          ...this.budget(request.operation)
        })
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
      await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.writeDescriptor({
          peerId: record.nativePeerId,
          selector,
          value,
          ticket,
          ...this.budget(request.operation)
        })
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
   * The CCCD requirement carried to the core, when this platform carries
   * one (see `deliveryRequirementToCore`). A preference is never a
   * requirement: the platform (the Windows adapter prefers notify) decides.
   */
  private coreRequirement(mode: SubscriptionOptions['deliveryMode']): 'notification' | 'indication' | null {
    if (!this.profile.deliveryRequirementToCore) return null
    if (mode === 'require-notification') return 'notification'
    if (mode === 'require-indication') return 'indication'
    return null
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
      const enabled = await this.withTicket(correlation, request.operation.signal, operation, ticket =>
        this.central.subscribe({
          peerId: record.nativePeerId,
          selector,
          consumer,
          ticket,
          ...(requirement === null ? {} : { deliveryMode: requirement }),
          overflowPolicy: request.options.delivery.overflowPolicy,
          ...this.budget(request.operation)
        })
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
        closed: false,
        pendingTerminal: null,
        surfacedLoss: 0,
        countersCheckedAt: -1,
        emptyPolls: 0
      }
      this.subscriptions.set(String(subscriptionId), subscription)
      record.subscriptions.add(String(subscriptionId))
      this.pumpNotifications(subscription).catch(error => this.failNotificationSource(subscription, error))
      return Object.freeze({
        subscriptionId,
        path,
        terminal: this.succeededTerminal(request.operation.correlation),
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
    error?: NormalizedBleError
  ): void {
    if (subscription.closed) return
    subscription.closed = true
    subscription.notifications.finishWithReason(reason, error ?? null)
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
        poll = await this.central.pollNotification({
          peerId: subscription.nativePeerId,
          selector: subscription.selector,
          consumer: subscription.consumer
        })
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
        subscription.notifications.emit(notificationValue(value, subscription.delivery), NOTIFICATION_BYTES)
        continue
      }
      if (poll.kind === 'terminal' && subscription.pendingTerminal === null) {
        // The core ended an `error` consumer on overflow, its own or values
        // lost upstream: the stream ends overflow with the core's counts, as
        // a local `error` overflow does.
        subscription.closed = true
        subscription.notifications.closeWithSourceOverflow({
          kind: 'overflow',
          policy: subscription.overflowPolicy,
          droppedItems: resourceCount(poll.droppedItems ?? 0),
          droppedBytes: resourceCount(poll.droppedBytes ?? 0),
          replacedItems: resourceCount(poll.replacedItems ?? 0)
        })
        return
      }
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
              : 'closed')
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
        await this.withTicket(correlation, operationOptions.signal, operation, ticket =>
          this.central.unsubscribe({
            peerId: stored.nativePeerId,
            selector: stored.selector,
            consumer: stored.consumer,
            ticket,
            ...this.budget(operationOptions)
          })
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
function notificationValue(value: OwnedBytes, delivery: 'notification' | 'indication' | 'unknown'): NotificationValue {
  return Object.freeze({ value, delivery })
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
    { uuid: string; occurrence: number; characteristics: Map<string, MutableCharacteristic> }
  >()
  const key = (uuid: string, occurrence: number): string => `${uuid}#${occurrence}`
  for (const path of paths) {
    if (typeof path.serviceUuid !== 'string' || !Number.isSafeInteger(path.serviceOccurrence)) {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const serviceUuid = uuidFromCore(path.serviceUuid, `${operation}.service`)
    let service = services.get(key(serviceUuid, path.serviceOccurrence))
    if (service === undefined) {
      service = { uuid: serviceUuid, occurrence: path.serviceOccurrence, characteristics: new Map() }
      services.set(key(serviceUuid, path.serviceOccurrence), service)
    }
    if (path.characteristicUuid === undefined || path.characteristicUuid === null) continue
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

function bytesToCore(value: Uint8Array, operation: string): Uint8Array {
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
 * plain read/write property → `none`; nothing reported → `unknown`.
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
    return permitted ? 'none' : 'unknown'
  }
  return Object.freeze({
    read: requirement(access.secureRead, access.encryptRead, access.encryptAuthenticatedRead, (bits & 0x01) !== 0),
    write: requirement(access.secureWrite, access.encryptWrite, access.encryptAuthenticatedWrite, (bits & 0x06) !== 0)
  })
}

/** Built-in feature ids the Rust provider can wire, when the core implements them on the OS. */
const CORE_BACKED_FEATURES = Object.freeze({
  rssi: BUILT_IN_FEATURE_IDS.connectionRssi,
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
  readonly rssi: boolean
  readonly maximumWriteLength: boolean
  readonly addressTargeting: boolean
  readonly security: boolean
  readonly maintainConnection: boolean
  readonly pairingGeneration: boolean
  readonly writeReadiness: boolean
}

/**
 * What the core implements on this profile's OS (the core's own capability
 * registration, read from the loaded binary), narrowed to what this
 * provider wires through to the contract.
 */
export function desktopRustCoreWiring(states: readonly DesktopRustCoreCapabilityState[]): DesktopRustCoreWiring {
  const limited = new Set(states.filter(row => row.state === 'limited').map(row => row.id))
  return Object.freeze({
    rssi: limited.has(CORE_BACKED_FEATURES.rssi),
    maximumWriteLength: limited.has(CORE_BACKED_FEATURES.maximumWriteLength),
    addressTargeting: limited.has(CORE_BACKED_FEATURES.addressTargeting),
    security: CORE_BACKED_FEATURES.security.every(id => limited.has(id)),
    maintainConnection: limited.has(BUILT_IN_FEATURE_IDS.backgroundDesktopMaintainConnection),
    pairingGeneration: limited.has(BUILT_IN_FEATURE_IDS.securityPairingGeneration),
    // The core registers readiness `limited` everywhere, but only an OS
    // with a real readiness signal answers the probe; elsewhere the row's
    // limitation says there is none, and a watch would be a false claim.
    writeReadiness: states.some(
      row =>
        row.id === BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness &&
        row.state === 'limited' &&
        row.limitation !== 'no-readiness-signal'
    )
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
  const registrations: FeatureRegistry['registrations'][number][] = [
    registration(BUILT_IN_FEATURE_IDS.connectionDirect, 'capability.catalog-v2'),
    registration(BUILT_IN_FEATURE_IDS.gattDescriptors, 'capability.catalog-v2')
  ]
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
    registrations.push(...createCoreBluetoothUnsupportedRegistrations(DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION))
  }
  return createFeatureRegistry(Object.freeze(registrations))
}

/** The scenario a desktop capability row's TCK suite runs to prove it. */
function desktopRustCoreSuiteScenarios(suiteId: string): readonly string[] {
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
