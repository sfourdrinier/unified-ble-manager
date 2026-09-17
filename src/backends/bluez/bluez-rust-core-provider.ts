// src/backends/bluez/bluez-rust-core-provider.ts
//
// R03 cutover: node-bluez shared-core provider. Manager creation, scan,
// connect, discover, read, write, subscribe, timeout, and dispose execute
// the shared Rust core (`DesktopCentral` over the production btleplug radio)
// through the NAPI `UbmCentral` dispatch (`bindings/napi/src/dispatch.rs`) —
// never the TypeScript BlueZ runtime.
//
// Every BLE data-path method below dispatches through an admitted
// `BluezRustCoreCentral`: args cross verbatim (deadlines as `timeoutMs` so
// the core owns the caller outcome; aborts map to core `cancelOperation`
// where the dispatch surface exposes the core operation id) and the raw
// core result returns. TypeScript schedules nothing, owns no subscription
// state, no retry policy, and no timeout timers. Core failures cross as JS
// `Error`s carrying the frozen `code|domain|operation|detail` wire form and
// propagate with the exact Rust-issued identity: this provider never
// substitutes a TypeScript error identity for a core one.
//
// A missing core fails loudly with `capability.unsupported` /
// `bluez-manager.rust-core-missing`: there is no silent TypeScript fallback,
// by design. The legacy `DbusNextBluezBoundaryFactory` transport stays
// exported for package-surface compatibility but is no longer on the public
// factory execution path (see `src/node-bluez.ts`).
//
// NAPI surface gaps this provider documents instead of hiding (each is a
// follow-up in `bindings/napi/src/dispatch.rs`, outside this module):
//   * no `adapter.state` op: adapter power reads `unknown` (the dispatch
//     surface exposes no power readout); availability tracks the open core.
//   * no `peers.*` ops: the optional peer directory is omitted; peer handles
//     enter through scan observations.
//   * no `counters.describe` op: `resourceCounters()` serves backend-tracked
//     lifecycle counts (documented per field), never core internals.
//   * no `events.take` op: backend events carry diagnostics only.
//   * per-op core operation ids surface only for scans: `requestCancellation`
//     cancels tracked scan ops through the core and reports
//     `not-cancellable` otherwise, never inventing an outcome.
//   * no pairing ceremony: `security` is absent and privileged
//     `pairingGeneration` controllers are rejected loudly at the factory.
//   * scan duplicate/merge policy is the core's (`all`/`none` admission in
//     `DesktopCentral`): the requested TS policies are not forwarded by the
//     dispatch surface, and non-service filters fail closed (below).

import { createRequire } from 'node:module'
import nodePath from 'node:path'
import { contractError, BackendContractError } from '../../backend-contract/errors'
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
import type { AdvertisementObservation, OwnerScanOptions, SourceTimestamp } from '../../backend-contract/advertisement'
import {
  canonicalUuid,
  capacity,
  createAttachmentBoundIdFactory,
  negotiateCoreVersions,
  opaqueId,
  resourceCount,
  type AttachmentBoundIdFactory,
  type ClientId,
  type LeaseId,
  type MonotonicTimestamp,
  type OwnedBytes,
  type PeerId,
  type ScanShareToken,
  type Uuid
} from '../../backend-contract/primitives'
import { createGattCharacteristicProperties } from '../../backend-contract/gatt'
import { createBackendOperationDispatch } from '../../backend-contract/operations'
import type {
  BackendOperationDispatch,
  CancellationAcknowledgement,
  OperationOptions,
  OperationTerminalRecord,
  PublicOperationOptions,
  ReadRequest,
  ReadResult,
  SubscribeRequest,
  WriteRequest,
  WriteResult
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
import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON } from './bluez-dbus-contract'
import { BLUEZ_PLATFORM_ID, bluezCompatibility } from './bluez-backend-provider'

export const BLUEZ_RUST_CORE_BACKEND_ID = 'unified-ble:bluez-rust-core'
export const BLUEZ_RUST_CORE_PROVIDER_ID = 'unified-ble:bluez-rust-core-provider'
export const BLUEZ_RUST_CORE_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION
export const BLUEZ_RUST_CORE_MISSING_OPERATION = 'bluez-manager.rust-core-missing'

/** Which radio the admitted central executes: production btleplug or the deterministic synthetic. */
export type BluezRustCoreRadio = 'production' | 'synthetic'

/** Selector accepted by the NAPI dispatch surface (occurrences select among duplicates). */
export interface BluezRustCoreSelector {
  readonly serviceUuid: string
  readonly serviceOccurrence?: number
  readonly characteristicUuid?: string
  readonly characteristicOccurrence?: number
  readonly descriptorUuid?: string
  readonly descriptorOccurrence?: number
}

/**
 * The `UbmCentral` JS surface this provider executes (see
 * `bindings/napi/src/dispatch.rs`). The default binding resolves it from the
 * dispatch addon; tests inject a faithful double. Staging methods are
 * deliberately absent: production traffic never stages.
 */
export interface BluezRustCoreCentral {
  startScan(options: { owner: string; serviceUuids: string[]; timeoutMs: number }): Promise<{ operationId: string }>
  stopScan(): Promise<void>
  takeAdvertisement(): Promise<BluezRustCoreAdvertisement | null>
  connect(options: { peerId: string; lease: string; timeoutMs: number }): Promise<{
    peerKey: string
    connectionGeneration: string | null
  }>
  disconnect(options: { peerId: string; lease: string }): Promise<void>
  discover(options: { peerId: string; lease: string }): Promise<{
    pathsRegistered: number
    skipped: ReadonlyArray<{ uuid: string; code: string }>
  }>
  discoveredPaths(peerId: string): Promise<BluezRustCorePath[]>
  read(options: { peerId: string; selector: BluezRustCoreSelector; timeoutMs: number }): Promise<Uint8Array>
  write(options: {
    peerId: string
    selector: BluezRustCoreSelector
    value: Uint8Array
    mode?: string
    timeoutMs: number
  }): Promise<void>
  readDescriptor(options: { peerId: string; selector: BluezRustCoreSelector; timeoutMs: number }): Promise<Uint8Array>
  writeDescriptor(options: {
    peerId: string
    selector: BluezRustCoreSelector
    value: Uint8Array
    timeoutMs: number
  }): Promise<void>
  subscribe(options: {
    peerId: string
    selector: BluezRustCoreSelector
    consumer: string
    timeoutMs: number
  }): Promise<void>
  takeNotification(options: {
    peerId: string
    selector: BluezRustCoreSelector
    consumer: string
  }): Promise<Uint8Array | null>
  unsubscribe(options: { peerId: string; selector: BluezRustCoreSelector; consumer: string }): Promise<boolean>
  cancelOperation(operationId: string): Promise<{ outcome: string; kind?: string | null; cause?: string | null }>
  close(): Promise<void>
}

export interface BluezRustCoreAdvertisement {
  readonly peerId: string
  readonly address?: string | null
  readonly rssi?: number | null
  readonly localName?: string | null
  readonly serviceUuids: readonly string[]
  readonly manufacturerData: ReadonlyArray<{ companyId: number; payload: unknown }>
  readonly serviceData: ReadonlyArray<{ uuid: string; payload: unknown }>
  readonly txPower?: number | null
}

export interface BluezRustCorePath {
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid?: string | null
  readonly characteristicOccurrence?: number | null
  readonly descriptorUuid?: string | null
  readonly descriptorOccurrence?: number | null
  readonly properties: number
}

/** Native Rust core entry: opens one dispatch central on the chosen radio. */
export interface BluezRustCoreBinding {
  openProduction(owner: string): Promise<BluezRustCoreCentral>
  openSynthetic(owner: string): Promise<BluezRustCoreCentral>
}

export interface BluezRustCoreProviderOptions {
  /** Owner label for the admitted core central (host identity). */
  readonly owner: string
  /** Monotonic clock supplied by the Node host. */
  readonly now: () => number
  /**
   * Radio selection. `production` (default) executes the btleplug radio and
   * fails loudly with `adapter.unavailable` where no adapter exists — it
   * never falls back to synthetic. `synthetic` is the hardware-free leg and
   * must be requested explicitly (tests); the public factory never selects it.
   */
  readonly radio?: BluezRustCoreRadio
  /**
   * Injected core entry. Absent, the provider resolves the dispatch addon
   * (loud `bluez-manager.rust-core-missing` when no addon loads): there is
   * no default and no TypeScript fallback.
   */
  readonly binding?: BluezRustCoreBinding
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
}

const NO_CALLER_DEADLINE_TIMEOUT_MS = 0xffffffff
const CORE_SCAN_OWNER = 'node-bluez'

/**
 * Reinterpret a dispatch rejection with the exact Rust-issued identity. The
 * NAPI layer renders every core failure as `code|domain|operation|detail`,
 * so the parse is verbatim: code, domain, and operation cross unchanged and
 * the detail rides as platform evidence. A rejection that is already a
 * contract error returns untouched; a non-wire failure (broken binding
 * transport) reports `platform.transport` loudly rather than inventing a
 * core identity.
 */
export function toBluezRustCoreError(error: unknown, fallbackOperation: string): never {
  if (error instanceof BackendContractError) throw error
  // Never `instanceof Error`: dispatch rejections may arrive cross-realm
  // (the addon builds them outside the caller realm), where prototype
  // identity fails but the `message` data property still reads. Duck-type it.
  const message =
    typeof (error as { message?: unknown } | null | undefined)?.message === 'string'
      ? String((error as { message: unknown }).message)
      : String(error)
  const parsed = /^([^|]+)\|([^|]+)\|([^|]+)\|(.*)$/s.exec(message)
  if (parsed !== null) {
    const [, code, domain, operation, detail] = parsed as unknown as [string, string, string, string, string]
    throw contractError(
      code as Parameters<typeof contractError>[0],
      domain as Parameters<typeof contractError>[1],
      operation.length > 0 ? operation : fallbackOperation,
      detail.length > 0
        ? { domain: 'bluez-rust-core', code: 'core-detail', safeMessage: detail, metadata: Object.freeze({}) }
        : null
    )
  }
  throw contractError('platform.transport', 'platform', fallbackOperation, {
    domain: 'bluez-rust-core',
    code: 'binding-rejection',
    safeMessage: message.slice(0, 512),
    metadata: Object.freeze({})
  })
}

function assertNonEmptyOwner(owner: string, operation: string): void {
  if (owner.length === 0) {
    throw contractError('argument.invalid', 'core', operation)
  }
}

const ADDON_ENV = 'UBM_NAPI_ADDON'

function packageBindingsDir(): string | null {
  // No `__dirname`/`import.meta` here: this module ships as both CJS and ESM
  // from the same source, and only one of those spellings parses per format.
  // Resolve the package root by self-reference (the package exports its own
  // `./package.json`), falling back to the process working directory (the
  // repository root when running this checkout's own tests).
  try {
    const scoped = createRequire(`${process.cwd()}/package.json`)
    const root = nodePath.dirname(scoped.resolve('unified-ble-manager/package.json'))
    return nodePath.join(root, 'bindings', 'napi')
  } catch {
    return null
  }
}

function addonCandidates(): string[] {
  const fromEnv = process.env[ADDON_ENV]
  // An explicitly set addon path is exclusive: a set-but-missing path fails
  // loudly instead of silently probing another artifact.
  if (typeof fromEnv === 'string' && fromEnv.length > 0) return [fromEnv]
  const platformSuffix = `${process.platform}-${process.arch}`
  const roots = [packageBindingsDir(), nodePath.join(process.cwd(), 'bindings', 'napi')].filter(
    (root): root is string => typeof root === 'string' && root.length > 0
  )
  const names = [`ubm_echo.${platformSuffix}.node`, 'ubm_echo.linux-x64.node']
  const candidates: string[] = []
  for (const root of roots) {
    for (const name of names) candidates.push(nodePath.join(root, name))
  }
  return candidates
}

function requireAddon(candidate: string): unknown {
  const globalRequire = (globalThis as { require?: (id: string) => unknown }).require
  if (typeof globalRequire === 'function') return globalRequire(candidate)
  return createRequire(`${process.cwd()}/package.json`)(candidate)
}

/**
 * Resolve the dispatch addon binding. Anything but a loadable addon whose
 * `UbmCentral` entry opens production and synthetic centrals fails with
 * `capability.unsupported` (`bluez-manager.rust-core-missing`): the caller
 * chose the core-backed factory explicitly, so a missing core is a loud
 * error, never a silent TypeScript substitution.
 */
export function resolveBluezRustCoreBinding(): BluezRustCoreBinding {
  const failures: string[] = []
  for (const candidate of addonCandidates()) {
    let addon: unknown
    try {
      addon = requireAddon(candidate)
    } catch (error) {
      const detail =
        typeof (error as { message?: unknown } | null | undefined)?.message === 'string'
          ? String((error as { message: unknown }).message)
          : String(error)
      failures.push(`${candidate}: ${detail}`)
      continue
    }
    const central = (addon as { UbmCentral?: unknown }).UbmCentral as
      | {
          open?: unknown
          openSynthetic?: unknown
        }
      | undefined
    if (
      central === undefined ||
      central === null ||
      typeof central.open !== 'function' ||
      typeof central.openSynthetic !== 'function'
    ) {
      failures.push(`${candidate}: UbmCentral.open/openSynthetic entry missing`)
      continue
    }
    const entry = central as {
      open(owner: string): Promise<BluezRustCoreCentral>
      openSynthetic(owner: string): Promise<BluezRustCoreCentral>
    }
    return Object.freeze({
      openProduction: async (owner: string) => {
        assertNonEmptyOwner(owner, 'bluez-manager.rust-core-owner')
        try {
          return await entry.open(owner)
        } catch (error) {
          toBluezRustCoreError(error, 'bluez-manager.rust-core-open')
        }
      },
      openSynthetic: async (owner: string) => {
        assertNonEmptyOwner(owner, 'bluez-manager.rust-core-owner')
        try {
          return await entry.openSynthetic(owner)
        } catch (error) {
          toBluezRustCoreError(error, 'bluez-manager.rust-core-open-synthetic')
        }
      }
    })
  }
  throw contractError('capability.unsupported', 'capability', BLUEZ_RUST_CORE_MISSING_OPERATION, {
    domain: 'bluez-rust-core',
    code: 'addon-missing',
    safeMessage: `no NAPI dispatch addon loads (set ${ADDON_ENV} to the built addon)`,
    metadata: Object.freeze({ failures: Object.freeze(failures.slice(0, 8)) })
  })
}

interface CoreDatabaseTree {
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

interface StoredCoreService {
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

interface StoredCoreDatabase {
  readonly tree: CoreDatabaseTree
  readonly base: import('../../backend-contract/gatt').DatabasePath<string, string, string>
  readonly services: StoredCoreService[]
}

/**
 * Creates the shared-core BlueZ provider. The core entry is injected by the
 * host (tests) or resolved from the dispatch addon; without either the
 * factory throws `capability.unsupported` (`bluez-manager.rust-core-missing`)
 * instead of constructing the legacy TypeScript transport.
 */
export function createBluezRustCoreBackendProvider(
  options: BluezRustCoreProviderOptions
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  if (options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', 'bluez-rust-core.provider.owner')
  }
  const binding = options.binding ?? resolveBluezRustCoreBinding()
  const radio = options.radio ?? 'production'
  const createOwnerId = options.createOwnerId ?? allocateRustCoreOwnerId
  return Object.freeze({
    descriptor: Object.freeze({
      providerId: BLUEZ_RUST_CORE_PROVIDER_ID,
      hostKind: 'node',
      loadability: 'loadable',
      compatibility: bluezCompatibility
    }),
    listAdapters: async () => {
      const backend = await openRustCoreBackend(options.owner, options.now, binding, radio, createOwnerId)
      try {
        return Object.freeze([backend.identity.attachment.adapter])
      } finally {
        await backend.destroy()
      }
    },
    create: async (selection: AdapterSelection<string>) => {
      const expected = rustCoreAdapterId(options.owner)
      if (String(selection.selectedAdapterId) !== String(expected)) {
        throw contractError('adapter.unavailable', 'adapter', 'bluez-rust-core.provider.select-adapter')
      }
      return openRustCoreBackend(options.owner, options.now, binding, radio, createOwnerId)
    }
  })
}

let nextRustCoreOwner = 1

function allocateRustCoreOwnerId(): string {
  const ordinal = nextRustCoreOwner
  nextRustCoreOwner += 1
  return `bluez-rust-core-owner-${ordinal}`
}

function rustCoreAdapterId(owner: string): string {
  return String(opaqueId(`bluez-rust-core-adapter:${owner}`, 'adapter', 'bluez-rust-core'))
}

async function openRustCoreBackend(
  owner: string,
  now: () => number,
  binding: BluezRustCoreBinding,
  radio: BluezRustCoreRadio,
  createOwnerId: () => string
): Promise<BluezRustCoreBackend> {
  const sessionOwner = createOwnerId()
  if (sessionOwner.length === 0) {
    throw contractError('argument.invalid', 'core', 'bluez-rust-core.provider.owner-id')
  }
  // Open-then-admit without leaking: a central that fails to open never
  // becomes a backend, and a backend that fails to open is destroyed before
  // the rejection propagates.
  let central: BluezRustCoreCentral
  try {
    central =
      radio === 'synthetic'
        ? await binding.openSynthetic(`${owner}/${sessionOwner}`)
        : await binding.openProduction(`${owner}/${sessionOwner}`)
  } catch (error) {
    if (error instanceof BackendContractError) throw error
    toBluezRustCoreError(error, 'bluez-manager.rust-core-open')
  }
  const backend = new BluezRustCoreBackend(owner, central, now, radio)
  try {
    await backend.open()
    return backend
  } catch (error) {
    await backend.destroy().catch(() => undefined)
    throw error
  }
}

/**
 * Shared-core backend: every BLE data-path method dispatches through the
 * admitted dispatch central. No timers, no retries, no subscription
 * bookkeeping live here — the core owns admission, deadlines, overflow, and
 * teardown. What TypeScript retains is transport mapping only: branded
 * identities, occurrence numerals, stream plumbing, and verbatim verdicts.
 */
export class BluezRustCoreBackend implements BleCentralBackend<string, HostNeutralBackendIdentity<string>> {
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly peers?: PeerDirectoryBackend<string> = undefined
  readonly security = undefined
  readonly features = createBluezRustCoreFeatureRegistry()

  private identifiers: AttachmentBoundIdFactory<string>
  private attachment: AttachmentRecord<string>
  private readonly peerIdsByNativeId = new Map<string, PeerId<string>>()
  private readonly nativeIdsByPeerId = new Map<string, string>()
  private nextPeer = 1
  private nextScan = 1
  private nextConnection = 1
  private nextLease = 1
  private nextOperation = 1
  private nextEventOrdinalValue = 1
  private destroyed = false
  private opened = false
  private destroyResult: Promise<import('../../backend-contract/errors').CleanupRecord> | null = null
  private readonly eventsStream: CoreBoundedStream<BackendEvent<string>>
  private readonly activeScanObservations = new Set<CoreBoundedStream<AdvertisementObservation<string>>>()
  private readonly activeNotificationStreams = new Set<
    BoundedAsyncStream<import('../../backend-contract/gatt').NotificationValue>
  >()
  private readonly activeAdapterTransitions = new Set<CoreBoundedStream<AdapterStateSnapshot<string>>>()
  private readonly adapterWatchers = new Set<(state: AdapterStateSnapshot<string>) => void>()
  private readonly connectionLeases = new Map<string, { lease: string; nativePeerId: string }>()
  private readonly scanOperationIds = new Map<string, string>()
  private readonly subscriptionConsumers = new Map<
    string,
    { nativePeerId: string; selector: BluezRustCoreSelector; consumer: string; closed: boolean }
  >()
  private readonly databases = new Map<string, StoredCoreDatabase>()
  private readonly occurrenceNumerals = new Map<string, number>()
  private dispatchedOperations = 0

  constructor(
    private readonly owner: string,
    private readonly central: BluezRustCoreCentral,
    private readonly now: () => number,
    private readonly radio: BluezRustCoreRadio
  ) {
    const attachmentId = opaqueId(`bluez-rust-core-attachment:${owner}`, 'attachment', 'bluez-rust-core')
    const backendInstanceId = opaqueId(`bluez-rust-core-backend:${owner}`, 'backend-instance', 'bluez-rust-core')
    const backendGeneration = opaqueId(`bluez-rust-core-generation:${owner}`, 'backend-generation', 'bluez-rust-core')
    const adapterGeneration = opaqueId(
      `bluez-rust-core-adapter-generation:${owner}`,
      'adapter-generation',
      'bluez-rust-core'
    )
    // The attachment tuple is frozen at open: identity equality covers the
    // adapter state (including updatedAt), so live radio state must never
    // rewrite it. Live state is served by adapter.currentState().
    this.attachment = Object.freeze({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapter: Object.freeze({
        adapterId: rustCoreAdapterId(owner) as AdapterDescriptor<string>['adapterId'],
        displayName: 'Shared-core desktop adapter',
        state: Object.freeze({
          availability: 'unknown' as const,
          authorization: 'unknown' as const,
          power: 'unknown' as const,
          backendGeneration,
          updatedAt: 0 as MonotonicTimestamp,
          safeReason: 'shared-core attachment state loads on open'
        }),
        adapterGeneration,
        limitations: Object.freeze([
          'The shared Rust core owns radio scheduling; this backend carries no TypeScript radio policy'
        ])
      })
    })
    this.identifiers = createAttachmentBoundIdFactory<string>({
      attachmentId,
      backendInstanceId,
      backendGeneration,
      adapterId: this.attachment.adapter.adapterId,
      adapterGeneration
    })
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
        throw contractError('capability.unsupported', 'scan', 'bluez-rust-core.scan.join')
      }
    })
    this.connections = Object.freeze({
      connect: (peerId: PeerId<string>, clientId: ClientId<string, string>, options: ConnectionOptions) =>
        this.connect(peerId, clientId, options)
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

  /** Loads the frozen attachment adapter state from the open core. */
  async open(): Promise<void> {
    this.attachment = Object.freeze({
      ...this.attachment,
      adapter: Object.freeze({
        ...this.attachment.adapter,
        state: Object.freeze({
          availability: 'available' as const,
          // BlueZ exposes no per-application authorization concept; the
          // dispatch surface reports nothing further, so both stay measured
          // unknowns with the reason stated, never a denial.
          authorization: 'unknown' as const,
          // The NAPI dispatch surface exposes no power readout.
          power: 'unknown' as const,
          backendGeneration: this.attachment.adapter.state.backendGeneration,
          updatedAt: this.now() as MonotonicTimestamp,
          safeReason: BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON
        })
      })
    })
    this.opened = true
  }

  get identity(): HostNeutralBackendIdentity<string> {
    return Object.freeze({
      registeredBackendId: BLUEZ_RUST_CORE_BACKEND_ID,
      registeredPlatformId: BLUEZ_PLATFORM_ID,
      attachment: this.attachment,
      versions: negotiateCoreVersions(bluezCompatibility, bluezCompatibility),
      runtime: Object.freeze({
        hostKind: 'node',
        implementationVersion: BLUEZ_RUST_CORE_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          boundary: 'bluez-rust-core-v1',
          transport: 'napi-UbmCentral',
          radio: this.radio
        })
      })
    })
  }

  async attach(
    _request: BackendAttachmentRequest
  ): Promise<BackendAttachment<string, HostNeutralBackendIdentity<string>>> {
    this.assertOperational('bluez-rust-core.attach')
    if (!this.opened) {
      throw contractError('lifecycle.invalid-state', 'core', 'bluez-rust-core.attach-before-open')
    }
    return Object.freeze({ attachment: this.attachment, identity: this.identity })
  }

  events(): BoundedAsyncStream<BackendEvent<string>> {
    return this.eventsStream
  }

  resourceCounters(): ResourceCounters {
    // Backend-tracked lifecycle counts only: the dispatch surface exposes no
    // core counters, so core-owned fields report the backend-visible truth
    // (no TS-side queue exists; restoration/IPC/chooser lanes are absent).
    // Every field below states what this backend owns; none restates core
    // internals as measured data.
    if (!this.opened) {
      throw contractError('lifecycle.invariant-violation', 'core', 'bluez-rust-core.counters-unavailable')
    }
    return Object.freeze({
      activeScanControllers: resourceCount(this.activeScanObservations.size),
      scanConsumers: resourceCount(this.activeScanObservations.size),
      chooserSessions: resourceCount(0),
      connectionLeases: resourceCount(this.connectionLeases.size),
      physicalLinks: resourceCount(this.connectionLeases.size),
      databaseSnapshots: resourceCount(this.databases.size),
      physicalCccdEnablements: resourceCount(this.subscriptionConsumers.size),
      subscriptionConsumers: resourceCount(this.subscriptionConsumers.size),
      queuedOperations: resourceCount(0),
      dispatchedOperations: resourceCount(this.dispatchedOperations),
      retainedByteBuffers: resourceCount(0),
      restorationRecords: resourceCount(0),
      orphanedIpcOwners: resourceCount(0)
    })
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

  private async destroyInternal(): Promise<import('../../backend-contract/errors').CleanupRecord> {
    this.destroyed = true
    this.adapterWatchers.clear()
    for (const pumpState of this.subscriptionConsumers.values()) {
      pumpState.closed = true
    }
    this.subscriptionConsumers.clear()
    this.scanOperationIds.clear()
    const ownedStreams: Array<{ close(): Promise<unknown> }> = [
      ...this.activeScanObservations,
      ...this.activeNotificationStreams,
      ...this.activeAdapterTransitions
    ]
    this.activeScanObservations.clear()
    this.activeNotificationStreams.clear()
    this.activeAdapterTransitions.clear()
    for (const stream of ownedStreams) {
      await stream.close().catch(() => undefined)
    }
    try {
      await this.central.close()
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      toBluezRustCoreError(error, 'bluez-rust-core.session.dispose')
    } finally {
      await this.eventsStream.close()
    }
    return { state: 'released', failures: [] }
  }

  private assertOperational(operation: string): void {
    if (this.destroyed) {
      throw contractError('lifecycle.destroyed', 'core', operation)
    }
  }

  private noteDispatch(): void {
    this.dispatchedOperations += 1
  }

  private nextEventOrdinal(): number {
    const ordinal = this.nextEventOrdinalValue
    this.nextEventOrdinalValue += 1
    return ordinal
  }

  /**
   * Map a caller deadline to the core `timeoutMs` arg. A null deadline means
   * the caller set no bound: the core holds the op until it completes or is
   * cancelled (the scan path's historical posture — run until stopped), so it
   * crosses as the widest u32. An expired deadline crosses as zero so the
   * core fails the op closed instead of executing unbounded work.
   */
  private timeoutMs(options: PublicOperationOptions): number {
    if (options.deadline === null || options.deadline === undefined) return NO_CALLER_DEADLINE_TIMEOUT_MS
    const remaining = Math.floor(Number(options.deadline) - this.now())
    if (!Number.isFinite(remaining)) return NO_CALLER_DEADLINE_TIMEOUT_MS
    return Math.min(Math.max(0, remaining), NO_CALLER_DEADLINE_TIMEOUT_MS)
  }

  private async requestCancellation(correlation: string): Promise<CancellationAcknowledgement<string>> {
    const handle = this.identifiers.backendOperationHandle(correlation)
    // Only scans surface their core operation id through the dispatch
    // surface; every other op reports not-cancellable rather than inventing
    // an outcome. The core still owns the completion either way.
    const operationId = this.scanOperationIds.get(correlation)
    if (operationId === undefined) {
      return { handle, state: 'not-cancellable' }
    }
    try {
      await this.central.cancelOperation(operationId)
      return { handle, state: 'cancellation-requested' }
    } catch {
      return { handle, state: 'not-cancellable' }
    }
  }

  /**
   * Attaches the abort listener BEFORE the core op starts so an abort can
   * never miss the window between dispatch and subscription. Returns a
   * remover the op must call when it settles, or every completed op leaks
   * one listener.
   */
  private watchAbort(signal: AbortSignal | null, onAbort: () => void): () => void {
    if (signal === null || signal === undefined) return () => undefined
    if (signal.aborted) {
      onAbort()
      return () => undefined
    }
    signal.addEventListener('abort', onAbort, { once: true })
    return () => signal.removeEventListener('abort', onAbort)
  }

  // -- adapter -----------------------------------------------------------

  private async currentAdapterState(): Promise<AdapterStateSnapshot<string>> {
    this.assertOperational('bluez-rust-core.adapter.state')
    if (!this.opened) {
      throw contractError('lifecycle.invalid-state', 'core', 'bluez-rust-core.adapter.state-before-open')
    }
    return Object.freeze({ ...this.attachment.adapter.state, updatedAt: this.now() as MonotonicTimestamp })
  }

  private async watchAdapterState(): Promise<AdapterStateWatch<string>> {
    const initial = await this.currentAdapterState()
    // The dispatch surface exposes no adapter-change queue, so transitions
    // fire only on backend lifecycle (destroy closes the stream). The
    // initial snapshot is the measured state; nothing is synthesized.
    const transitions = new CoreBoundedStream<AdapterStateSnapshot<string>>(
      { itemCapacity: capacity(16), byteCapacity: capacity(4096), reservedControlCapacity: capacity(512) },
      'drop-oldest'
    )
    this.activeAdapterTransitions.add(transitions)
    return Object.freeze({ initial, transitions })
  }

  // -- peers ---------------------------------------------------------------

  private peerIdForNativeId(nativePeerId: string): PeerId<string> {
    const existing = this.peerIdsByNativeId.get(nativePeerId)
    if (existing !== undefined) return existing
    const peerId = opaqueId(`bluez-core-peer-${this.nextPeer}`, 'peer', 'bluez-rust-core')
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

  // -- scan ------------------------------------------------------------------

  private async startScan(
    options: OwnerScanOptions<string, string>,
    _clientId: ClientId<string, string>
  ): Promise<ScanLease<string, string>> {
    this.assertOperational('bluez-rust-core.scan.start')
    const serviceUuids = options.filter.serviceUuids.map(service => String(service))
    // The dispatch surface filters by service UUIDs only: richer filters
    // cannot execute in the core and must fail closed, never silently match
    // everything. Duplicate/merge policy is core-owned (see module header).
    if (options.filter.manufacturerData.length > 0 || options.filter.localNamePrefix !== null) {
      throw contractError('capability.unsupported', 'scan', 'bluez-rust-core.scan.filter')
    }
    if (options.filter.deviceAddresses !== undefined && options.filter.deviceAddresses.length > 0) {
      throw contractError('capability.unsupported', 'scan', 'bluez-rust-core.scan.filter')
    }
    const ordinal = this.nextScan
    this.nextScan += 1
    this.noteDispatch()
    const correlation = String(this.mintedCorrelation('scan'))
    const owner = `${this.owner}/${CORE_SCAN_OWNER}-${ordinal}`
    let started: { operationId: string }
    try {
      started = await this.central.startScan({
        owner,
        serviceUuids,
        timeoutMs: this.timeoutMs(options)
      })
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      toBluezRustCoreError(error, 'bluez-rust-core.scan.start')
    }
    if (typeof started.operationId !== 'string' || started.operationId.length === 0) {
      throw contractError('protocol.malformed', 'core', 'bluez-rust-core.scan.start.shape')
    }
    // Track the core operation id under the caller correlation so aborts
    // cancel through the core instead of stranding the scan.
    this.scanOperationIds.set(correlation, started.operationId)
    const scanSessionId = this.identifiers.scanSessionId(`bluez-core-scan-session-${ordinal}`)
    const leaseId = this.identifiers.leaseId(`bluez-core-scan-lease-${ordinal}`)
    const shareToken =
      options.sharing.mode === 'owner' && options.sharing.allowSharing
        ? this.identifiers.scanShareToken(`bluez-core-scan-share-${ordinal}`)
        : null
    const observations = new CoreBoundedStream<AdvertisementObservation<string>>(
      options.delivery,
      options.delivery.overflowPolicy
    )
    this.activeScanObservations.add(observations)
    let stopped = false
    const stop = async (): Promise<import('../../backend-contract/errors').CleanupRecord> => {
      if (stopped) return { state: 'released', failures: [] }
      stopped = true
      this.scanOperationIds.delete(correlation)
      try {
        try {
          await this.central.stopScan()
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.scan.stop')
        }
      } finally {
        this.activeScanObservations.delete(observations)
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
        if (isStopped() || this.destroyed) return
        let next: BluezRustCoreAdvertisement | null
        try {
          next = await this.central.takeAdvertisement()
        } catch {
          // Transport failure ends the pump through stop (core verdicts, not
          // pump guesses); a stopped/destroyed backend just exits.
          if (!isStopped() && !this.destroyed) {
            await stop().catch(() => undefined)
          }
          return
        }
        if (next === null || next === undefined) {
          await pumpDelay()
          continue
        }
        let observation: AdvertisementObservation<string>
        try {
          observation = this.mapObservation(next, scanSessionId)
        } catch {
          // One malformed core record must not tear down the scan: skip it
          // with a diagnostic trace and keep pumping.
          this.noteSkippedCoreRecord()
          continue
        }
        observations.emit(observation, 512)
      }
    } catch {
      if (!isStopped() && !this.destroyed) {
        await stop().catch(() => undefined)
      }
    }
  }

  /**
   * Evidence for a skipped malformed core record: lifecycle facts the typed
   * surface cannot express ride as diagnostics, so the skip is never silent.
   */
  private noteSkippedCoreRecord(): void {
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

  private mapObservation(
    value: BluezRustCoreAdvertisement,
    scanSessionId: import('../../backend-contract/primitives').ScanSessionId<string, string>
  ): AdvertisementObservation<string> {
    if (typeof value.peerId !== 'string' || value.peerId.length === 0) {
      throw contractError('protocol.malformed', 'core', 'bluez-rust-core.scan.observation.peer')
    }
    const peerId = this.peerIdForNativeId(value.peerId)
    return Object.freeze({
      device: Object.freeze({
        id: peerId,
        backendInstanceId: this.attachment.backendInstanceId,
        scope: 'session',
        stableAcrossRestarts: false,
        address:
          typeof value.address === 'string' && value.address.length > 0
            ? Object.freeze({ value: value.address, type: 'opaque' as const })
            : null
      }),
      provenance: 'platform-raw',
      sourceTimestamp: absentField<SourceTimestamp>('source timestamp not reported by the dispatch surface'),
      receivedAtMonotonicMs: this.now() as AdvertisementObservation<string>['receivedAtMonotonicMs'],
      ingressOrdinal: 0 as AdvertisementObservation<string>['ingressOrdinal'],
      scanSessionId,
      localName: presentField<string>(typeof value.localName === 'string' ? value.localName : null),
      rssi: presentField<number>(typeof value.rssi === 'number' ? value.rssi : null),
      txPower: presentField<number>(typeof value.txPower === 'number' ? value.txPower : null),
      connectable: absentField<boolean>('connectable not reported by the dispatch surface'),
      appearance: absentField<number>('appearance not reported by the dispatch surface'),
      serviceUuids: presentField<readonly Uuid[]>(
        Array.isArray(value.serviceUuids)
          ? Object.freeze(
              (value.serviceUuids as unknown[]).map(entry =>
                uuidFromCore(String(entry), 'bluez-rust-core.scan.service-uuids')
              )
            )
          : null
      ),
      solicitedServiceUuids: absentField<readonly Uuid[]>('solicited services not reported by the dispatch surface'),
      overflowServiceUuids: absentField<readonly Uuid[]>('overflow services not reported by the dispatch surface'),
      serviceData: presentField(
        Array.isArray(value.serviceData)
          ? Object.freeze(
              (value.serviceData as Array<{ uuid: string; payload: unknown }>).map(entry =>
                Object.freeze({
                  serviceUuid: uuidFromCore(String(entry.uuid), 'bluez-rust-core.scan.service-data'),
                  value: ownedBytes(bytesFromCore(entry.payload))
                })
              )
            )
          : null
      ),
      manufacturerData: presentField(
        Array.isArray(value.manufacturerData)
          ? Object.freeze(
              (value.manufacturerData as Array<{ companyId: number; payload: unknown }>).map(entry =>
                Object.freeze({
                  companyIdentifier: Number(entry.companyId),
                  value: ownedBytes(bytesFromCore(entry.payload))
                })
              )
            )
          : null
      ),
      rawRecord: absentField<import('../../backend-contract/primitives').OwnedBytes>(
        'raw record not reported by the dispatch surface'
      ),
      scanResponseRecord: absentField<import('../../backend-contract/primitives').OwnedBytes>(
        'scan response record not reported by the dispatch surface'
      )
    })
  }

  // -- connections -----------------------------------------------------------

  private async connect(
    peerId: PeerId<string>,
    _clientId: ClientId<string, string>,
    options: ConnectionOptions
  ): Promise<ConnectionLease<string, string, string>> {
    this.assertOperational('bluez-rust-core.connection.connect')
    const nativePeerId = this.nativeIdForPeerId(peerId, 'bluez-rust-core.connection.unknown-peer')
    // The dispatch surface connects directly over LE: intents, transports,
    // and PHY selections it cannot express fail closed instead of silently
    // degrading to a direct connection.
    if (options.intent !== undefined && options.intent !== 'direct') {
      throw contractError('capability.unsupported', 'connection', 'bluez-rust-core.connection.intent')
    }
    if (options.transport !== undefined && options.transport !== 'le' && options.transport !== 'auto') {
      throw contractError('argument.invalid', 'connection', 'bluez-rust-core.connection.transport')
    }
    if (options.preferredPhy !== undefined && options.preferredPhy.length > 0) {
      throw contractError('capability.unsupported', 'connection', 'bluez-rust-core.connection.phy')
    }
    const ordinal = this.nextConnection
    this.nextConnection += 1
    const leaseOrdinal = this.nextLease
    this.nextLease += 1
    const lease = `bluez-core-lease-${leaseOrdinal}`
    this.noteDispatch()
    // No caller correlation crosses the dispatch connect (the surface takes
    // no operation id), so an already-aborted caller fails before dispatch
    // and a racing abort cannot cancel by id: the core still owns the
    // completion, and disconnect/release settles the link.
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', 'bluez-rust-core.connection.connect.aborted')
    }
    let connected: { peerKey: string; connectionGeneration: string | null }
    try {
      connected = await this.central.connect({
        peerId: nativePeerId,
        lease,
        timeoutMs: this.timeoutMs(options)
      })
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      toBluezRustCoreError(error, 'bluez-rust-core.connection.connect')
    }
    if (typeof connected.peerKey !== 'string' || typeof connected.connectionGeneration !== 'string') {
      throw contractError('protocol.malformed', 'core', 'bluez-rust-core.connection.connect.shape')
    }
    const connectionId = this.identifiers.connectionId(`bluez-core-connection-${ordinal}`)
    const leaseId = this.identifiers.leaseId(`bluez-core-connection-lease-${leaseOrdinal}`)
    const connectionGeneration = opaqueId(
      String(connected.connectionGeneration),
      'connection-generation',
      'bluez-rust-core'
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
    try {
      await this.central.disconnect({ peerId: nativePeerId, lease })
    } catch (error) {
      if (error instanceof BackendContractError) throw error
      toBluezRustCoreError(error, 'bluez-rust-core.connection.disconnect')
    }
    return { state: 'released', failures: [] }
  }

  // -- GATT ------------------------------------------------------------------

  private selectorFor(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    operation: string
  ): BluezRustCoreSelector {
    const record = path as unknown as Record<string, unknown>
    for (const field of ['serviceUuid', 'characteristicUuid'] as const) {
      if (typeof record[field] !== 'string') {
        throw contractError('protocol.malformed', 'core', `${operation}.selector`)
      }
    }
    return {
      serviceUuid: record.serviceUuid as string,
      serviceOccurrence: this.occurrenceNumeral(String(record.serviceOccurrence), `${operation}.service-occurrence`),
      characteristicUuid: record.characteristicUuid as string,
      characteristicOccurrence: this.occurrenceNumeral(
        String(record.characteristicOccurrence),
        `${operation}.characteristic-occurrence`
      )
    }
  }

  private descriptorSelectorFor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    operation: string
  ): BluezRustCoreSelector {
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
    throw contractError('peer.not-found', 'connection', operation)
  }

  private succeededTerminal(
    correlation: OperationTerminalRecord<string, string>['correlation']
  ): OperationTerminalRecord<string, string> {
    return Object.freeze({ correlation, outcome: 'succeeded', cause: null })
  }

  private mintOccurrence(kind: string, numeral: number): string {
    // Occurrence identities are decimal strings of the core numeral (the
    // portable snapshot layer requires `/^(0|[1-9][0-9]*)$/`): the brand
    // carries scope, the value stays the numeral.
    const id = String(opaqueId(String(numeral), kind, 'bluez-rust-core'))
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

  private mintedCorrelation(
    kind: string
  ): import('../../backend-contract/primitives').OperationCorrelation<string, string> {
    const ordinal = this.nextOperation
    this.nextOperation += 1
    return this.identifiers.operationCorrelation(`bluez-core-${kind}-${ordinal}`)
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

  private async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    this.assertOperational('bluez-rust-core.gatt.discover')
    const nativePeerId = this.nativeIdForPeerId(connection.peerId, 'bluez-rust-core.gatt.discover.peer')
    // Never address the core with an empty lease: without a live
    // connection lease the handle is stale and the core cannot route it.
    const leaseEntry = this.connectionLeases.get(String(connection.connectionId))
    if (leaseEntry === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', 'bluez-rust-core.gatt.discover.lease')
    }
    this.noteDispatch()
    const lease = leaseEntry.lease
    const correlation = String(this.mintedCorrelation('discover'))
    const removeAbort = this.watchAbort(options.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    let tree: CoreDatabaseTree
    try {
      let report: { pathsRegistered: number; skipped: ReadonlyArray<{ uuid: string; code: string }> }
      try {
        report = await this.central.discover({ peerId: nativePeerId, lease })
      } catch (error) {
        if (error instanceof BackendContractError) throw error
        toBluezRustCoreError(error, 'bluez-rust-core.gatt.discover')
      }
      let paths: BluezRustCorePath[]
      try {
        paths = await this.central.discoveredPaths(nativePeerId)
      } catch (error) {
        if (error instanceof BackendContractError) throw error
        toBluezRustCoreError(error, 'bluez-rust-core.gatt.discovered-paths')
      }
      // Partial discovery is a core success with skips, never a silent short
      // snapshot: each skipped entry rides the backend event stream as a
      // diagnostic so the report is never dropped quietly.
      for (const _skipped of report.skipped) {
        this.noteSkippedCoreRecord()
      }
      tree = groupCorePaths(paths, 'bluez-rust-core.gatt.discover.shape')
    } finally {
      removeAbort()
    }
    const databaseOrdinal = this.nextOperation
    this.nextOperation += 1
    const databaseId = this.identifiers.databaseId(`bluez-core-database-${databaseOrdinal}`)
    const databaseGeneration = opaqueId(
      `bluez-core-database-generation-${databaseOrdinal}`,
      'database-generation',
      'bluez-rust-core'
    )
    const path = Object.freeze({
      attachment: this.attachment,
      attachmentId: this.attachment.attachmentId,
      peerId: connection.peerId,
      connectionId: connection.connectionId,
      ownerLeaseId: this.identifiers.leaseId(`bluez-core-database-lease-${databaseOrdinal}`),
      connectionGeneration: connection.connectionGeneration,
      databaseId,
      databaseGeneration
    })
    // Mint stable occurrence identities once per discovery: snapshot paths
    // flow back into this.gatt.* unchanged, and the numerals map back to
    // the core selector occurrences for the wire.
    const stored: StoredCoreDatabase = { tree, base: path, services: [] }
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

  private storedDatabase(path: { databaseId: unknown }, operation: string): StoredCoreDatabase {
    const stored = this.databases.get(String(path.databaseId))
    if (stored === undefined) {
      throw contractError('gatt.stale-handle', 'gatt', operation)
    }
    return stored
  }

  private async databaseSnapshot(
    path: GattDatabase<string, string, string>['path']
  ): Promise<import('../../backend-contract/gatt').GattDatabaseSnapshot<string, string, string>> {
    this.assertOperational('bluez-rust-core.gatt.snapshot')
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.snapshot')
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
        serviceUuid: uuidFromCore(entry.service.uuid, 'bluez-rust-core.gatt.snapshot.service'),
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
            'bluez-rust-core.gatt.snapshot.characteristic'
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
                descriptorUuid: uuidFromCore(descriptor.uuid, 'bluez-rust-core.gatt.snapshot.descriptor'),
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
    stored: StoredCoreDatabase,
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

  private async databaseRead(
    path: GattDatabase<string, string, string>['path'],
    characteristic: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: PublicOperationOptions
  ): Promise<import('../../backend-contract/primitives').OwnedBytes> {
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.database-read')
    const resolved = this.resolveCharacteristic(stored, characteristic, 'bluez-rust-core.gatt.database-read')
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
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.database-write')
    const resolved = this.resolveCharacteristic(stored, characteristic, 'bluez-rust-core.gatt.database-write')
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
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.database-read-descriptor')
    const resolved = this.resolveCharacteristic(stored, descriptor, 'bluez-rust-core.gatt.database-read-descriptor')
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
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.database-write-descriptor')
    const resolved = this.resolveCharacteristic(stored, descriptor, 'bluez-rust-core.gatt.database-write-descriptor')
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
    const stored = this.storedDatabase(path, 'bluez-rust-core.gatt.database-subscribe')
    const resolved = this.resolveCharacteristic(stored, characteristic, 'bluez-rust-core.gatt.database-subscribe')
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

  private read(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.assertOperational('bluez-rust-core.gatt.read')
    const selector = this.selectorFor(path, 'bluez-rust-core.gatt.read')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'bluez-rust-core.gatt.read.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    this.noteDispatch()
    const removeAbort = this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<ReadResult<string, string>> => {
      try {
        let raw: Uint8Array
        try {
          raw = await this.central.read({ peerId: nativePeerId, selector, timeoutMs })
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.gatt.read')
        }
        return Object.freeze({
          value: ownedBytes(bytesFromCore(raw)),
          terminal: this.succeededTerminal(request.operation.correlation)
        })
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }

  private write(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.assertOperational('bluez-rust-core.gatt.write')
    const selector = this.selectorFor(path, 'bluez-rust-core.gatt.write')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'bluez-rust-core.gatt.write.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const value = bytesToCore(request.bytes)
    const mode = request.mode
    this.noteDispatch()
    const removeAbort = this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<WriteResult<string, string>> => {
      try {
        try {
          await this.central.write({ peerId: nativePeerId, selector, value, mode, timeoutMs })
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.gatt.write')
        }
        return Object.freeze({
          terminal: this.succeededTerminal(request.operation.correlation),
          commitState: 'confirmed'
        })
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }

  private readDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.assertOperational('bluez-rust-core.gatt.read-descriptor')
    const selector = this.descriptorSelectorFor(path, 'bluez-rust-core.gatt.read-descriptor')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'bluez-rust-core.gatt.read-descriptor.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    this.noteDispatch()
    const removeAbort = this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<ReadResult<string, string>> => {
      try {
        let raw: Uint8Array
        try {
          raw = await this.central.readDescriptor({ peerId: nativePeerId, selector, timeoutMs })
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.gatt.read-descriptor')
        }
        return Object.freeze({
          value: ownedBytes(bytesFromCore(raw)),
          terminal: this.succeededTerminal(request.operation.correlation)
        })
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }

  private writeDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.assertOperational('bluez-rust-core.gatt.write-descriptor')
    const selector = this.descriptorSelectorFor(path, 'bluez-rust-core.gatt.write-descriptor')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'bluez-rust-core.gatt.write-descriptor.peer'
    )
    // The dispatch surface writes descriptors without a mode: only the
    // default with-response write can execute, anything else fails closed.
    if (request.mode !== 'with-response') {
      throw contractError('capability.unsupported', 'gatt', 'bluez-rust-core.gatt.write-descriptor.mode')
    }
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const value = bytesToCore(request.bytes)
    this.noteDispatch()
    const removeAbort = this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<WriteResult<string, string>> => {
      try {
        try {
          await this.central.writeDescriptor({ peerId: nativePeerId, selector, value, timeoutMs })
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.gatt.write-descriptor')
        }
        return Object.freeze({
          terminal: this.succeededTerminal(request.operation.correlation),
          commitState: 'confirmed'
        })
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }

  private subscribe(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, string>
  ): BackendOperationDispatch<string, BackendSubscription<string, string, string, string, string>> {
    this.assertOperational('bluez-rust-core.gatt.subscribe')
    const selector = this.selectorFor(path, 'bluez-rust-core.gatt.subscribe')
    const nativePeerId = this.nativePeerForPath(
      path as unknown as { peerId?: unknown },
      'bluez-rust-core.gatt.subscribe.peer'
    )
    const correlation = String(request.operation.correlation)
    const timeoutMs = this.timeoutMs(request.operation)
    const consumerOrdinal = this.nextOperation
    this.nextOperation += 1
    const consumer = `bluez-core-consumer-${consumerOrdinal}`
    this.noteDispatch()
    const removeAbort = this.watchAbort(request.operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<BackendSubscription<string, string, string, string, string>> => {
      try {
        try {
          await this.central.subscribe({ peerId: nativePeerId, selector, consumer, timeoutMs })
        } catch (error) {
          if (error instanceof BackendContractError) throw error
          toBluezRustCoreError(error, 'bluez-rust-core.gatt.subscribe')
        }
        const subscriptionId = this.identifiers.subscriptionId(`bluez-core-subscription-${consumerOrdinal}`)
        const notifications = new CoreBoundedStream<import('../../backend-contract/gatt').NotificationValue>(
          request.options.delivery,
          request.options.delivery.overflowPolicy
        )
        this.activeNotificationStreams.add(notifications)
        const pumpState = { nativePeerId, selector, consumer, closed: false }
        this.subscriptionConsumers.set(String(subscriptionId), pumpState)
        const pump = (async (): Promise<void> => {
          try {
            for (;;) {
              if (pumpState.closed || this.destroyed) return
              let next: Uint8Array | null
              try {
                next = await this.central.takeNotification({ peerId: nativePeerId, selector, consumer })
              } catch {
                if (!pumpState.closed && !this.destroyed) {
                  await notifications.close().catch(() => undefined)
                }
                return
              }
              if (next === null || next === undefined) {
                await pumpDelay()
                continue
              }
              let value: import('../../backend-contract/primitives').OwnedBytes
              try {
                value = ownedBytes(bytesFromCore(next))
              } catch {
                this.noteSkippedCoreRecord()
                continue
              }
              notifications.emit(
                Object.freeze({
                  value,
                  indication: false
                }),
                512
              )
            }
          } catch {
            if (!pumpState.closed && !this.destroyed) {
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
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }

  private unsubscribe(
    subscription: BackendSubscription<string, string, string, string, string>,
    operation: OperationOptions<string, string>
  ): BackendOperationDispatch<string, OperationTerminalRecord<string, string>> {
    this.assertOperational('bluez-rust-core.gatt.unsubscribe')
    const stored = this.subscriptionConsumers.get(String(subscription.subscriptionId))
    const correlation = String(operation.correlation)
    this.noteDispatch()
    const removeAbort = this.watchAbort(operation.signal, () => {
      this.requestCancellation(correlation).catch(() => undefined)
    })
    const completion = (async (): Promise<OperationTerminalRecord<string, string>> => {
      try {
        if (stored !== undefined) {
          stored.closed = true
          try {
            try {
              await this.central.unsubscribe({
                peerId: stored.nativePeerId,
                selector: stored.selector,
                consumer: stored.consumer
              })
            } catch (error) {
              if (error instanceof BackendContractError) throw error
              toBluezRustCoreError(error, 'bluez-rust-core.gatt.unsubscribe')
            }
          } finally {
            this.subscriptionConsumers.delete(String(subscription.subscriptionId))
          }
        }
        this.activeNotificationStreams.delete(subscription.notifications)
        await subscription.notifications.close().catch(() => undefined)
        return this.succeededTerminal(operation.correlation)
      } finally {
        removeAbort()
      }
    })()
    return this.dispatchFor(correlation, completion)
  }
}

/**
 * Group the flat dispatch discovery paths into the database tree the backend
 * contract expects. Service-level paths (no characteristic) open the service
 * node; characteristic paths attach below their service; descriptor paths
 * attach below their characteristic. Anything else fails closed: inventing
 * tree structure the core did not report would corrupt every later selector.
 */
function groupCorePaths(paths: BluezRustCorePath[], operation: string): CoreDatabaseTree {
  const services = new Map<
    string,
    {
      uuid: string
      occurrence: number
      characteristics: Map<
        string,
        {
          uuid: string
          occurrence: number
          properties: number
          descriptors: Map<string, { uuid: string; occurrence: number }>
        }
      >
    }
  >()
  const serviceKey = (uuid: string, occurrence: number): string => `${uuid}#${occurrence}`
  for (const path of paths) {
    if (typeof path.serviceUuid !== 'string' || typeof path.serviceOccurrence !== 'number') {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const serviceUuid = uuidFromCore(path.serviceUuid, `${operation}.service`)
    const key = serviceKey(serviceUuid, Math.floor(path.serviceOccurrence))
    let service = services.get(key)
    if (service === undefined) {
      service = { uuid: serviceUuid, occurrence: Math.floor(path.serviceOccurrence), characteristics: new Map() }
      services.set(key, service)
    }
    if (path.characteristicUuid === undefined || path.characteristicUuid === null) continue
    if (typeof path.characteristicOccurrence !== 'number') {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const characteristicUuid = uuidFromCore(path.characteristicUuid, `${operation}.characteristic`)
    const characteristicKey = serviceKey(characteristicUuid, Math.floor(path.characteristicOccurrence))
    let characteristic = service.characteristics.get(characteristicKey)
    if (characteristic === undefined) {
      characteristic = {
        uuid: characteristicUuid,
        occurrence: Math.floor(path.characteristicOccurrence),
        properties: Math.floor(Number(path.properties) || 0),
        descriptors: new Map()
      }
      service.characteristics.set(characteristicKey, characteristic)
    }
    if (path.descriptorUuid === undefined || path.descriptorUuid === null) continue
    if (typeof path.descriptorOccurrence !== 'number') {
      throw contractError('protocol.malformed', 'core', operation)
    }
    const descriptorUuid = uuidFromCore(path.descriptorUuid, `${operation}.descriptor`)
    const descriptorKey = serviceKey(descriptorUuid, Math.floor(path.descriptorOccurrence))
    if (!characteristic.descriptors.has(descriptorKey)) {
      characteristic.descriptors.set(descriptorKey, {
        uuid: descriptorUuid,
        occurrence: Math.floor(path.descriptorOccurrence)
      })
    }
  }
  return {
    services: [...services.values()].map(service => ({
      uuid: service.uuid,
      occurrence: service.occurrence,
      characteristics: [...service.characteristics.values()].map(characteristic => ({
        uuid: characteristic.uuid,
        occurrence: characteristic.occurrence,
        properties: characteristic.properties,
        descriptors: [...characteristic.descriptors.values()]
      }))
    }))
  }
}

/** Accepts in-process bytes or `{ base64 }` from out-of-process doubles. */
function bytesFromCore(value: unknown): Uint8Array {
  if (value instanceof Uint8Array) return value
  if (typeof Buffer !== 'undefined' && Buffer.isBuffer(value)) return new Uint8Array(value)
  if (Array.isArray(value)) {
    // Reject out-of-range elements loudly: Uint8Array.from would wrap
    // them modulo 256 and silently corrupt the value.
    for (const entry of value as unknown[]) {
      if (typeof entry !== 'number' || !Number.isInteger(entry) || entry < 0 || entry > 255) {
        throw contractError('protocol.malformed', 'core', 'bluez-rust-core.bytes')
      }
    }
    return Uint8Array.from(value as number[])
  }
  if (typeof value === 'object' && value !== null && typeof (value as { base64?: unknown }).base64 === 'string') {
    const binary = Buffer.from((value as { base64: string }).base64, 'base64')
    return new Uint8Array(binary)
  }
  throw contractError('protocol.malformed', 'core', 'bluez-rust-core.bytes')
}

function bytesToCore(value: Uint8Array): Uint8Array {
  // Outbound bytes are BorrowedBytes: accept the typed array (Buffers
  // included) and reject anything else rather than coercing garbage.
  if (!(value instanceof Uint8Array)) {
    throw contractError('argument.invalid', 'gatt', 'bluez-rust-core.bytes')
  }
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
      reason: 'not reported by the dispatch surface',
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
 * Consumer-side delivery pacing between core `take` polls. This paces
 * delivery only: admission, deadlines, overflow, and teardown all stay
 * core-owned.
 */
function pumpDelay(): Promise<void> {
  return new Promise<void>(resolve => {
    const timer = setTimeout(resolve, 5)
    // Delivery pacing must never hold the process (or a test runner) open.
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

export function createBluezRustCoreFeatureRegistry() {
  const scenarioIds = Object.freeze(['scenario.scan-connect-discover-read-notify-destroy'])
  const ids: readonly BuiltInFeatureId[] = Object.freeze([
    BUILT_IN_FEATURE_IDS.connectionDirect,
    BUILT_IN_FEATURE_IDS.gattDescriptors
  ])
  return createFeatureRegistry(
    Object.freeze(
      ids.map(id =>
        createBackendOperationCapabilityRegistration({
          id,
          implementationVersion: BLUEZ_RUST_CORE_IMPLEMENTATION_VERSION,
          sourceDigest: `bluez-rust-core-${id.replace(':', '-')}-v1`,
          tckSuiteId: 'capability.catalog-v2',
          requiredScenarioIds: [...scenarioIds],
          operation: `${id}.invoke-without-rust-core-dispatch`
        })
      )
    )
  )
}
