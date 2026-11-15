// src/web.ts — zero-plumbing Web Bluetooth factory (PR1 final, no compatibility aliases)

import { assertPublicChooseOptions, snapshotBlePeer } from './public/ble-manager'
import type { BleManager, ChooseFilter, ChooseOptions } from './public/ble-manager'
import { createPublicBleManager } from './public/ble-manager'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import {
  createWebBluetoothProvider,
  WEB_BLUETOOTH_ADAPTER_ID,
  type WebBluetoothBackend
} from './web/web-bluetooth-backend'
import {
  createDefaultNavigatorWebBluetoothEnvironment,
  NavigatorWebBluetoothBoundary,
  type NavigatorWebBluetoothEnvironment
} from './web/navigator-web-bluetooth-boundary'
import { createBleManagerFromBackend, DEFAULT_BLE_MANAGER_OPTIONS } from './manager/ble-manager'
import { byteLimit, opaqueId } from './backend-contract/primitives'
import { createEphemeralHostIdentity } from './public/host-identity'
import { canonicalUuidInput } from './backend-contract/primitives'
import type { Uuid } from './backend-contract/primitives'
import { normalizeOperationOptions } from './public/operation-options'
import { contractError } from './backend-contract/errors'
import { rehydratePublicPromise } from './public/error-bridge'
import type { WebContinuationRecordingController } from './web/web-clear-attempt'
import {
  currentDefaultWebRecordingDatabase,
  defaultWebRecordingPartition,
  webRecordingAvailable,
  webRecordingDependencies
} from './web/web-recording-partition'
import type { WebRecordingPartition } from './web/web-recording-partition'
import type { WebRecordingStartDependencies } from './web/web-ordered-recording-start'
import { acquireWebRecordingLock } from './web/web-recording-lock'
import type { WebRecordingLockManager } from './web/web-recording-lock'
import { createWebRecordingRecovery } from './web/web-recording-recovery'
import { createWebTerminalEvidenceSource } from './web/web-terminal-evidence'
import type { WebTerminalEvidenceRecoveryEntry } from './web/web-terminal-evidence'
import { trustedWebRecordingError, webStorageError, webRecordingContractError } from './web/web-recording-errors'
import type { WebRecordingRecovery } from './web/web-recording-recovery'

// Active Web backends are scoped to the app-provided durable partition. This
// is deliberately not global recovery: callers can retry only the partition
// whose retained evidence they chose to own.
const terminalEvidenceBackends = new WeakMap<WebRecordingPartition, Set<WebBluetoothBackend>>()
const terminalEvidenceProducers: readonly ('scan' | 'subscription')[] = Object.freeze(['scan', 'subscription'])

function terminalEvidenceReferenceDigest(reference: string): string {
  const digest = /^:ubm-terminal-evidence-v1:([0-9a-f]{64})$/.exec(reference)?.[1]
  if (digest === undefined)
    throw webRecordingContractError('protocol.malformed', 'stream', 'web-terminal-evidence.recovery-reference')
  return digest
}

/** Retry retained terminal-evidence commits after the application's storage recovers. */
export async function retryWebTerminalEvidence(partition: WebRecordingPartition): Promise<void> {
  const backends = terminalEvidenceBackends.get(partition)
  if (backends !== undefined) {
    for (const backend of [...backends]) await backend.retryPendingTerminalEvidence()
  }
  const recording = webRecordingDependencies(partition)
  if (recording.terminalEvidenceDigest === undefined) return
  await createWebTerminalEvidenceSource({
    database: recording.database,
    locks: recording.locks,
    digest: recording.digest,
    digestBytes: recording.terminalEvidenceDigest,
    // Retry only reads and finalizes retained rows; the reservation quota is
    // already durably fixed in terminalEvidenceState.
    maximumRetainedBytes: recording.terminalEvidenceMaximumDurableBytes ?? Number.MAX_SAFE_INTEGER
  }).retryPendingTerminalEvidence()
}

/** Partition-scoped inspection and explicit retirement of retained terminal evidence. */
export async function createWebTerminalEvidenceRecovery(
  partition: WebRecordingPartition
): Promise<WebTerminalEvidenceRecovery> {
  const recording = webRecordingDependencies(partition)
  if (recording.terminalEvidenceDigest === undefined)
    throw webRecordingContractError('capability.unsupported', 'stream', 'web-terminal-evidence.unavailable')
  const source = createWebTerminalEvidenceSource({
    database: recording.database,
    locks: recording.locks,
    digest: recording.digest,
    digestBytes: recording.terminalEvidenceDigest,
    maximumRetainedBytes: recording.terminalEvidenceMaximumDurableBytes ?? Number.MAX_SAFE_INTEGER
  })
  return Object.freeze({
    list: () => source.listTerminalEvidence(),
    async listWithStatus() {
      if (recording.locks === undefined)
        throw webRecordingContractError('capability.unsupported', 'stream', 'web-terminal-evidence.recovery-lock')
      const busy = new Set<'scan' | 'subscription'>()
      const records: WebTerminalEvidenceRecoveryEntry[] = []
      for (const record of await source.listTerminalEvidence()) {
        const digest = terminalEvidenceReferenceDigest(record.ref)
        const lock = await acquireWebRecordingLock(
          recording.locks,
          async () => digest,
          `terminal-${digest.slice(0, 32)}`
        )
        if (lock.state === 'busy') {
          busy.add(record.producer)
          continue
        }
        await lock.release()
        records.push(record)
      }
      return Object.freeze({
        records: Object.freeze(records),
        busyProducers: Object.freeze(terminalEvidenceProducers.filter(producer => busy.has(producer)))
      })
    },
    read: (ref: string, maximumBytes: number) => {
      const read = source.readTerminalEvidence
      if (read === undefined)
        throw webRecordingContractError('capability.unsupported', 'stream', 'web-terminal-evidence.read')
      return read(ref, maximumBytes)
    },
    acknowledge: (ref: string) => {
      const release = source.releaseTerminalEvidence
      if (release === undefined)
        throw webRecordingContractError('capability.unsupported', 'stream', 'web-terminal-evidence.release')
      return release(ref)
    },
    retry: () => retryWebTerminalEvidence(partition)
  })
}

export interface WebTerminalEvidenceRecovery {
  list(): Promise<readonly WebTerminalEvidenceRecoveryEntry[]>
  /**
   * Lists rows whose Web Lock is currently available together with producers
   * that have one or more live-lock-owned rows. `list()` remains the complete
   * inspection view and does not acquire locks.
   */
  listWithStatus(): Promise<
    Readonly<{
      readonly records: readonly WebTerminalEvidenceRecoveryEntry[]
      readonly busyProducers: readonly ('scan' | 'subscription')[]
    }>
  >
  read(
    ref: string,
    maximumBytes: number
  ): Promise<
    | { readonly kind: 'committed'; readonly canonicalBytes: Readonly<Uint8Array>; readonly digest: string }
    | { readonly kind: 'missing' | 'unresolved' }
  >
  acknowledge(ref: string): Promise<void>
  retry(): Promise<void>
}

declare const crypto: WebRecordingStartDependencies['crypto']

export { createWebRecordingPartition } from './web/web-recording-partition'
export type { WebRecordingPartition, WebRecordingPartitionOptions } from './web/web-recording-partition'
export type {
  WebClearAttemptV1,
  WebClearAttemptReceiptV1,
  WebRecordingRuntimeDiagnostic,
  WebContinuationRecordingController
} from './web/web-clear-attempt'
export type { WebTerminalEvidenceRecoveryEntry } from './web/web-terminal-evidence'
export type {
  WebProofAdmission,
  WebProofOutcome,
  WebProofUsage,
  WebProofClearResult
} from './web/web-clear-proof-ledger'

export type {
  WebRecordingRecovery,
  WebClearProof,
  WebClearReceipt,
  WebRecordingRecoveryInspection,
  WebRecordingOperationEvidence
} from './web/web-recording-recovery'

/** Inspect and explicitly dispose retained evidence without changing ownership or dispatching work. */
export async function createWebRecordingRecoveryInspector(
  partition?: WebRecordingPartition
): Promise<WebRecordingRecovery> {
  const publicRecovery = <T>(work: Promise<T>): Promise<T> =>
    rehydratePublicPromise(
      work.catch(error => {
        throw trustedWebRecordingError(error) ?? webStorageError(error, 'indexeddb-transaction')
      })
    )
  return publicRecovery(
    (async () => {
      const selected = partition ?? (await defaultWebRecordingPartition())
      const recovery = createWebRecordingRecovery(webRecordingDependencies(selected).database)
      return {
        listClearProofs: options => publicRecovery(Promise.resolve().then(() => recovery.listClearProofs(options))),
        getClearAttemptV1: (attempt, options) =>
          publicRecovery(Promise.resolve().then(() => recovery.getClearAttemptV1(attempt, options))),
        acknowledgeClearAttemptV1: (attempt, receipt) =>
          publicRecovery(Promise.resolve().then(() => recovery.acknowledgeClearAttemptV1(attempt, receipt))),
        listIncarnations: (id, options) =>
          publicRecovery(Promise.resolve().then(() => recovery.listIncarnations(id, options))),
        listMarkerOperations: (id, incarnation, options) =>
          publicRecovery(Promise.resolve().then(() => recovery.listMarkerOperations(id, incarnation, options))),
        disposeRetained: (id, incarnation, options) =>
          publicRecovery(Promise.resolve().then(() => recovery.disposeRetained(id, incarnation, options))),
        inspect: (id, incarnation) => publicRecovery(Promise.resolve().then(() => recovery.inspect(id, incarnation))),
        listMarkers: (id, incarnation, options) =>
          publicRecovery(Promise.resolve().then(() => recovery.listMarkers(id, incarnation, options))),
        reconcileMarkerOperation: (id, incarnation, operationId) =>
          publicRecovery(Promise.resolve().then(() => recovery.reconcileMarkerOperation(id, incarnation, operationId)))
      }
    })()
  )
}

/** Controller for the same browser storage partition used by the default factories. */
export async function createWebRecordingController(): Promise<WebContinuationRecordingController> {
  const withController = <T>(run: (controller: WebContinuationRecordingController) => Promise<T>): Promise<T> =>
    rehydratePublicPromise(
      defaultWebRecordingPartition()
        .then(partition => partition.createController())
        .then(run)
    )
  return rehydratePublicPromise(
    defaultWebRecordingPartition().then(() => ({
      status: id => withController(controller => controller.status(id)),
      runtimeDiagnostic: id => withController(controller => controller.runtimeDiagnostic(id)),
      prepare: (id, options) => withController(controller => controller.prepare(id, options)),
      prepareOrdered: (id, options) => withController(controller => controller.prepareOrdered(id, options)),
      acknowledge: (id, token) => withController(controller => controller.acknowledge(id, token)),
      stop: id => withController(controller => controller.stop(id)),
      stopOrdered: id => withController(controller => controller.stopOrdered(id)),
      contextOrdered: id => withController(controller => controller.contextOrdered(id)),
      clear: id => withController(controller => controller.clear(id)),
      admitClearAttemptV2: identity => {
        const snapshot = { ...identity }
        return withController(controller => controller.admitClearAttemptV2(snapshot))
      },
      commitClearAttemptV2: (attempt, options) => {
        const snapshot = { ...attempt },
          signal = options?.signal
        return withController(controller => controller.commitClearAttemptV2(snapshot, { signal }))
      },
      inspectClearAttemptV2: attempt => {
        const snapshot = { ...attempt }
        return withController(controller => controller.inspectClearAttemptV2(snapshot))
      },
      cancelClearAttemptV2: attempt => {
        const snapshot = { ...attempt }
        return withController(controller => controller.cancelClearAttemptV2(snapshot))
      },
      retireClearAttemptV2: options => {
        const terminal = options.outcome
        const snapshot = {
          ...options,
          outcome:
            terminal.state === 'committed'
              ? { ...terminal, attempt: { ...terminal.attempt }, result: { ...terminal.result } }
              : { ...terminal, attempt: { ...terminal.attempt } }
        }
        return withController(controller => controller.retireClearAttemptV2(snapshot))
      },
      clearProofUsageV2: () => withController(controller => controller.clearProofUsageV2()),
      configureClearProofV2: options => {
        const snapshot = { ...options }
        return withController(controller => controller.configureClearProofV2(snapshot))
      },
      prepareClearAttemptV1: id => withController(controller => controller.prepareClearAttemptV1(id)),
      commitClearAttemptV1: (attempt, options) =>
        withController(controller => controller.commitClearAttemptV1(attempt, options)),
      clearAttemptV1: (attempt, options) => withController(controller => controller.clearAttemptV1(attempt, options))
    }))
  )
}

export {
  createWebBluetoothProvider,
  WEB_BLUETOOTH_ADAPTER_ID,
  WebBluetoothBackend,
  WebBluetoothProvider
} from './web/web-bluetooth-backend'
export {
  NavigatorWebBluetoothBoundary,
  createDefaultNavigatorWebBluetoothEnvironment
} from './web/navigator-web-bluetooth-boundary'
export function createNavigatorWebBluetoothProvider(environment: NavigatorWebBluetoothEnvironment) {
  return createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(environment))
}
export type { NavigatorWebBluetoothEnvironment } from './web/navigator-web-bluetooth-boundary'
export type { ChooserRequest, ChooserSelection, WebChooser, WebHost } from './backend-contract/host/web'
export type { WebRecordingPeerIdentityStore, WebRecordingPeerReference } from './web/web-recording-peer-identity'
export type {
  WebBluetoothAvailabilityListener,
  WebBluetoothBoundary,
  WebBluetoothCharacteristicBoundary,
  WebBluetoothCharacteristicProperties,
  WebBluetoothDescriptorBoundary,
  WebBluetoothDeviceBoundary,
  WebBluetoothDeviceSelection,
  WebBluetoothDisconnectListener,
  WebBluetoothGattServerBoundary,
  WebBluetoothNotificationListener,
  WebBluetoothPageLifecycleReason,
  WebBluetoothRequestDeviceOptions,
  WebBluetoothRequestFilter,
  WebBluetoothRequestManufacturerDataFilter,
  WebBluetoothServiceBoundary,
  WebBluetoothTimerHandle
} from './web/web-bluetooth-boundary'

// Zero-plumbing Web factory — returns one BleManager. No provider/clientId tuple.
export async function createWebBleManager(options: BleManagerCreateOptions = {}): Promise<BleManager> {
  return rehydratePublicPromise(
    (async () => {
      const normalized = normalizeWebCreateOptions(options)
      const env = createDefaultNavigatorWebBluetoothEnvironment()
      return createWebManager(env, normalized)
    })()
  )
}

// Explicit provider injection for tests and unusual hosts.
export interface WebBleManagerWithEnvironmentOptions {
  readonly environment: NavigatorWebBluetoothEnvironment
  readonly createOptions?: BleManagerCreateOptions
  readonly recordingPartition?: WebRecordingPartition
}

export async function createWebBleManagerWithEnvironment(
  options: WebBleManagerWithEnvironmentOptions
): Promise<BleManager> {
  return rehydratePublicPromise(
    (async () => {
      const env = options.environment
      const normalized = normalizeWebCreateOptions(options.createOptions)
      return createWebManager(env, normalized, options.recordingPartition)
    })()
  )
}

async function createWebManager(
  env: NavigatorWebBluetoothEnvironment,
  options: BleManagerCreateOptions,
  partition?: WebRecordingPartition
): Promise<BleManager> {
  const recordingDependencies =
    partition === undefined
      ? recordingStorageExposed()
        ? lazyDefaultRecordingDependencies()
        : undefined
      : webRecordingDependencies(partition)
  const recording =
    recordingDependencies === undefined
      ? undefined
      : {
          ...recordingDependencies,
          ...(options.terminalEvidence === undefined
            ? {}
            : { terminalEvidenceMaximumDurableBytes: options.terminalEvidence.maximumDurableBytes })
        }
  const provider = createWebBluetoothProvider(new NavigatorWebBluetoothBoundary(env), recording)
  const ephemeral = createEphemeralHostIdentity({ randomBytes: options.randomBytes })
  const instanceSuffix = options.instanceId === undefined ? '' : `-${options.instanceId}`
  const clientId = opaqueId(`web-${ephemeral.managerNonce}${instanceSuffix}`, 'client', 'web-bluetooth:browser')
  const managerId = opaqueId(`web-${ephemeral.attachmentNonce}${instanceSuffix}`, 'manager', 'web-bluetooth:browser')
  const backend = await provider.create({ selectedAdapterId: WEB_BLUETOOTH_ADAPTER_ID })
  if (partition !== undefined && options.terminalEvidence !== undefined) {
    let backends = terminalEvidenceBackends.get(partition)
    if (backends === undefined) {
      backends = new Set()
      terminalEvidenceBackends.set(partition, backends)
    }
    backends.add(backend)
  }
  const internal = await createBleManagerFromBackend(
    backend,
    {
      coreCompatibility: provider.descriptor.compatibility,
      manager: { clientId, managerId, ownerMode: 'owning' }
    },
    {
      ...DEFAULT_BLE_MANAGER_OPTIONS,
      now: env.now,
      maximumValueBytes:
        options.diagnostics?.maximumValueBytes === undefined
          ? DEFAULT_BLE_MANAGER_OPTIONS.maximumValueBytes
          : byteLimit(options.diagnostics.maximumValueBytes),
      traceMaximumRecords: options.diagnostics?.traceMaximumRecords ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumRecords,
      traceMaximumBytes: options.diagnostics?.traceMaximumBytes ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumBytes,
      terminalEvidence: options.terminalEvidence
    }
  )
  const publicManager = await createPublicBleManager(internal, env.now, {
    discoveryKind: 'system-chooser',
    choose: optionsValue => chooseWebPeer(backend, optionsValue, env.now)
  })
  if (partition === undefined || options.terminalEvidence === undefined) return publicManager
  return new Proxy(publicManager, {
    get(target, property, receiver) {
      if (property !== 'destroy') {
        const value = Reflect.get(target, property, receiver)
        return typeof value === 'function' ? value.bind(target) : value
      }
      return async () => {
        try {
          return await target.destroy()
        } finally {
          const backends = terminalEvidenceBackends.get(partition)
          backends?.delete(backend)
          if (backends?.size === 0) terminalEvidenceBackends.delete(partition)
        }
      }
    }
  })
}

function recordingStorageExposed(): boolean {
  try {
    return webRecordingAvailable()
  } catch {
    // Some hosts throw when storage is accessed. Defer that failure to the
    // recording request, provided the recording entropy source is present.
    return typeof crypto !== 'undefined'
  }
}

/** Defer opening storage until authority preparation, while retaining the
 * default partition's database and live-owner registry for every consumer. */
function lazyDefaultRecordingDependencies(): WebRecordingStartDependencies {
  class LazyDependencies implements WebRecordingStartDependencies {
    private resolved: WebRecordingStartDependencies | undefined
    readonly forRecording = (): WebRecordingStartDependencies => new LazyDependencies()
    readonly crypto = crypto
    readonly locks: WebRecordingLockManager = {
      request: (name, options, callback) => {
        const locks = this.current().locks
        if (locks === undefined)
          throw webRecordingContractError(
            'capability.unsupported',
            'restoration',
            'web-recording.no-exclusive-coordinator'
          )
        return locks.request(name, options, callback)
      }
    }
    readonly database: WebRecordingStartDependencies['database'] = {
      transaction: (stores, mode, options) => {
        this.current()
        return currentDefaultWebRecordingDatabase().transaction(stores, mode, options)
      }
    }
    readonly preparePartition = async (signal?: AbortSignal): Promise<void> => {
      const partition = await defaultWebRecordingPartition(signal)
      this.resolved = webRecordingDependencies(partition)
      if (this.resolved.locks === undefined)
        throw webRecordingContractError(
          'capability.unsupported',
          'restoration',
          'web-recording.no-exclusive-coordinator'
        )
    }
    readonly digest = (id: string): Promise<string> => this.current().digest(id)
    readonly terminalEvidenceDigest = (value: Readonly<Uint8Array>): Promise<string> => {
      const digest = this.current().terminalEvidenceDigest
      if (digest === undefined)
        throw webRecordingContractError('capability.unsupported', 'stream', 'web-terminal-evidence.exact-digest')
      return digest(value)
    }
    get liveOwners() {
      return this.current().liveOwners
    }
    private current(): WebRecordingStartDependencies {
      if (this.resolved === undefined)
        throw webRecordingContractError(
          'lifecycle.invariant-violation',
          'restoration',
          'web-recording.partition-not-ready'
        )
      return this.resolved
    }
  }
  return new LazyDependencies()
}

function normalizeWebCreateOptions(options: BleManagerCreateOptions | undefined): BleManagerCreateOptions {
  const normalized = normalizeBleManagerCreateOptions(options)
  if (normalized.adapterId !== undefined && normalized.adapterId !== WEB_BLUETOOTH_ADAPTER_ID) {
    throw contractError('adapter.unavailable', 'adapter', 'web-manager.adapter')
  }
  if (normalized.restoration !== undefined) {
    throw contractError('capability.unsupported', 'restoration', 'web-manager.restoration')
  }
  if (normalized.background !== undefined) {
    throw contractError('capability.unsupported', 'capability', 'web-manager.background')
  }
  return normalized
}

async function chooseWebPeer(backend: WebBluetoothBackend, options: ChooseOptions, now: () => number) {
  assertPublicChooseOptions(options)
  const normalized = normalizeOperationOptions(options, now)
  const filters = options.filters ?? []
  const optionalServices = (options.optionalServices ?? []).map(normalizeChooserUuid)
  const browserFilters = filters.map(normalizeChooserFilter)
  const acceptAllDevices = options.acceptAllDevices ?? browserFilters.length === 0
  const selection = await backend.choose(
    {
      filters: browserFilters,
      acceptAllDevices,
      optionalServices
    },
    normalized
  )
  const selected = backend.peerReferenceFor(String(selection.peerId))
  if (selected === null) throw contractError('protocol.violation', 'connection', 'web.choose.peer-reference')
  return snapshotBlePeer({
    id: String(selection.peerId),
    // Web Bluetooth exposes no advertisement payload for chooser devices:
    // the browser's `BluetoothDevice.name` is the only name source, and
    // `null` means the browser withheld it.
    name: backend.selectedPeerName(String(selection.peerId)),
    rssi: null,
    reference: { version: 1, backendId: selected.backendId, scope: 'origin', opaqueId: selected.browserDeviceId },
    sources: ['origin-authorized'],
    lastAdvertisement: null,
    state: {
      reachability: 'unknown',
      connection: 'disconnected',
      bond: 'unsupported',
      lastSeenAtMonotonicMs: null
    }
  })
}

function normalizeChooserUuid(value: string | number): Uuid {
  return canonicalUuidInput(value)
}

function normalizeChooserFilter(filter: ChooseFilter) {
  return {
    serviceUuids: (filter.serviceUuids ?? []).map(normalizeChooserUuid),
    manufacturerData: (filter.manufacturerData ?? []).map(manufacturer => ({
      companyIdentifier: manufacturer.companyIdentifier,
      dataPrefix: manufacturer.dataPrefix === undefined ? null : new Uint8Array(manufacturer.dataPrefix)
    })),
    localNamePrefix: filter.localNamePrefix ?? null
  }
}
