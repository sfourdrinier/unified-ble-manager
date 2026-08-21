// src/public/ble-manager.ts — non-generic application façade (PR1 skeleton)

import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { ScanOptions as InternalScanOptions } from '../backend-contract/advertisement'
import { contractError, type CleanupRecord } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import { opaqueId } from '../backend-contract/primitives'
import type { BleManager as InternalBleManager } from '../manager/ble-manager'
import type { BleManagerOptions } from '../manager/ble-manager'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import { resolveStreamPreset } from './stream-presets'
import type { StreamPreset } from './stream-presets'
import type { IpcAdvertisement } from '../ipc/manager'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from './error-bridge'
import { PublicBleCapabilities } from './capabilities'
import type { BleCapabilities } from './capabilities'
import type { BleAdapter, BleAdapterState, AdapterReadinessOptions } from './ble-adapter'
import type { BleDiagnostics } from './diagnostics'
import { snapshotResourceCounters } from './diagnostics'
import { isAuthorizationBlocking, type AdapterStateSnapshot } from '../backend-contract/identity'
import { createPublicGattDatabase } from './gatt'
import type { GattDatabase, GattValueEvent } from './gatt'
import { normalizeScanObservation, normalizeScanQuery, observationMatchesScanQuery, type ScanQuery } from './scan-query'
import type { BoundedAsyncStreamIterator } from '../backend-contract/streams'
import { createScanState } from './scan-state'

export type GattSubscriptionValue = GattValueEvent
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

// Public peer — opaque backend-scoped identifier, no generic.
export interface BlePeer {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
}

export function snapshotBlePeer(peer: BlePeer): BlePeer {
  return Object.freeze({ id: peer.id, name: peer.name, rssi: peer.rssi })
}

// Public connection — generation-bound lease, no generic.
export interface BleConnection {
  readonly peer: BlePeer
  readonly discover: (options?: OperationOptions) => Promise<GattDatabase>
  readonly disconnect: () => Promise<CleanupRecord>
  readonly release: () => Promise<CleanupRecord>
}

// Public scan session — bounded stream, no generic.
// Union embraces both native AdvertisementObservation and Tauri IpcAdvertisement
// until PR4 scan semantics unify; covariance lets each backend stream satisfy the union without casts.
export type PublicScanObservation = AdvertisementObservation<string> | IpcAdvertisement

export interface ScanSession {
  readonly stop: () => Promise<CleanupRecord>
  readonly observations: BoundedAsyncStream<PublicScanObservation>
  readonly state: AsyncIterable<ScanStateEvent>
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
  readonly discovery: BleDiscoveryInfo
  readonly destroy: () => Promise<CleanupRecord>
  scan(options?: ScanOptions): Promise<ScanSession>
  find(options?: FindOptions): Promise<BlePeer>
  choose(options?: ChooseOptions): Promise<BlePeer>
  connect(peer: BlePeer | string, options?: OperationOptions): Promise<BleConnection>
  withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    action: (connection: BleConnection) => Promise<T>
  ): Promise<T>
  withScan<T>(options: ScanOptions, action: (scan: ScanSession) => Promise<T>): Promise<T>
  withDiscoveredConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    action: (scope: { readonly connection: BleConnection; readonly gatt: GattDatabase }) => Promise<T>
  ): Promise<T>
}

export { PublicBleManager as BleManagerImpl }

export interface ScanOptions extends OperationOptions {
  readonly query?: ScanQuery
  readonly duplicates?: 'coalesced' | 'all'
  readonly delivery?: StreamPreset
}

export interface FindOptions extends OperationOptions {
  /** Defaults to 10 seconds when omitted. */
  readonly query?: ScanQuery
  readonly select?: 'first' | ((peer: BlePeer) => boolean)
}

export interface ChooseOptions extends OperationOptions {
  readonly services?: readonly (string | number)[]
}

export interface BleDiscoveryInfo {
  readonly kind: 'continuous-scan' | 'system-chooser' | 'hybrid'
}

export interface PublicBleManagerHostOptions {
  readonly discoveryKind?: BleDiscoveryInfo['kind']
  readonly choose?: (options: ChooseOptions) => Promise<BlePeer>
}

// Internal factory used by host entrypoints. Hosts derive identity and call this.
export async function createPublicBleManager(
  internal: InternalBleManager<string, BackendIdentity<string>>,
  now: () => number,
  hostOptions: PublicBleManagerHostOptions = {}
): Promise<BleManager> {
  return new PublicBleManager(internal, now, hostOptions)
}

class PublicBleManager implements BleManager {
  readonly capabilities: BleCapabilities
  readonly adapter: BleAdapter
  readonly diagnostics: BleDiagnostics

  constructor(
    private readonly internal: InternalBleManager<string, BackendIdentity<string>>,
    private readonly now: () => number,
    hostOptions: PublicBleManagerHostOptions
  ) {
    this.capabilities = new PublicBleCapabilities(internal)
    this.adapter = createPublicAdapter(internal, now)
    this.diagnostics = {
      snapshot: () =>
        Object.freeze({ trace: internal.traceDocument(), resourceCounters: this.diagnostics.resourceCounters() }),
      resourceCounters: () =>
        snapshotResourceCounters(
          Object.fromEntries(
            Object.entries(internal.localResourceCounters()).map(([key, value]) => [key, Number(value)])
          )
        ),
      startTrace: () => ({ stop: async () => internal.traceDocument() })
    }
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
      const preset = options.delivery ?? 'balanced'
      const delivery = resolveStreamPreset({ preset })
      const normalizedQuery = normalizeScanQuery(options.query)
      const internalOptions: InternalScanOptions<string, string> = {
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
        duplicatePolicy: options.duplicates === 'all' ? 'all' : 'merged',
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
      return {
        stop: async () => {
          scanState.emit({ state: 'stopping' })
          try {
            const cleanup = await rehydratePublicPromise(session.stop())
            scanState.emit({ state: 'stopped' })
            scanState.close()
            return cleanup
          } catch (error) {
            scanState.emit({ state: 'failed', reason: 'scan-stop-failed' })
            scanState.close()
            throw error
          }
        },
        observations: filterScanObservations(session.observations, normalizedQuery),
        state: scanState.stream
      }
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async find(options: FindOptions = {}): Promise<BlePeer> {
    const scan = await this.scan({
      ...options,
      query: options.query,
      duplicates: 'coalesced',
      delivery: 'latest',
      timeoutMs: options.timeoutMs ?? 10_000
    })
    return runWithCleanup(
      () => findPeerInScan(scan, options.select),
      () => scan.stop()
    )
  }

  choose(options: ChooseOptions = {}): Promise<BlePeer> {
    if (this.chooseImpl === undefined) {
      return Promise.reject(
        rehydratePublicError(contractError('capability.unsupported', 'chooser', 'public-ble-manager.choose'))
      )
    }
    return this.chooseImpl(options)
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<BleConnection> {
    try {
      const { signal, deadline } = normalizeOperationOptions(options, this.now)
      const peerIdString = typeof peer === 'string' ? peer : peer.id
      const peerId = opaqueId<'peer', string>(peerIdString, 'peer', 'public-ble-manager')
      const internalConnection = await this.internal.connect(peerId, {
        signal,
        deadline
      })
      const publicPeer =
        typeof peer === 'string' ? snapshotBlePeer({ id: peerIdString, name: null, rssi: null }) : snapshotBlePeer(peer)
      return {
        peer: publicPeer,
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
        disconnect: () => rehydratePublicPromise(internalConnection.disconnect()),
        release: () => rehydratePublicPromise(internalConnection.release())
      }
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
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
    peer: BlePeer | string,
    options: OperationOptions,
    action: (scope: { readonly connection: BleConnection; readonly gatt: GattDatabase }) => Promise<T>
  ): Promise<T> {
    return this.withConnection(peer, options, async connection => {
      const gatt = await connection.discover(options)
      return action(Object.freeze({ connection, gatt }))
    })
  }

  destroy(): Promise<CleanupRecord> {
    return this.internal.destroy().catch(error => {
      throw rehydratePublicError(error)
    })
  }
}

// Re-export for host factories that need the internal type.
export type { BleManagerOptions }

function createPublicAdapter(
  internal: InternalBleManager<string, BackendIdentity<string>>,
  now: () => number
): BleAdapter {
  const identity = internal.identity
  const adapterId = identity?.attachment?.adapter?.adapterId
  return {
    id: typeof adapterId === 'string' ? adapterId : null,
    state: async () => snapshotPublicAdapterState(await internal.adapterState()),
    waitUntilReady: options => waitForPublicAdapter(internal, now, options)
  }
}

async function waitForPublicAdapter(
  internal: InternalBleManager<string, BackendIdentity<string>>,
  now: () => number,
  options: AdapterReadinessOptions = {}
): Promise<BleAdapterState> {
  try {
    const normalized = normalizeOperationOptions(options, now)
    const deadline = normalized.deadline ?? now() + 10_000
    const watch = await internal.adapterStates({ signal: normalized.signal })
    const iterator = watch.values[Symbol.asyncIterator]()
    try {
      let current = watch.initial
      while (true) {
        assertAdapterCanBecomeReady(current, options.operation ?? 'scan')
        if (adapterIsReady(current)) return snapshotPublicAdapterState(current)
        if (normalized.signal?.aborted === true)
          throw contractError('operation.aborted', 'adapter', 'public-adapter.wait-until-ready')
        if (now() >= deadline) throw contractError('operation.timed-out', 'adapter', 'public-adapter.wait-until-ready')
        const item = await nextAdapterState(iterator, deadline - now())
        if (item.done) throw contractError('stream.closed', 'adapter', 'public-adapter.wait-until-ready')
        if (item.value.kind === 'terminal')
          throw contractError('stream.closed', 'adapter', 'public-adapter.wait-until-ready')
        if (item.value.kind === 'overflow') continue
        current = item.value.value
      }
    } finally {
      await watch.stop()
    }
  } catch (error) {
    throw rehydratePublicError(error)
  }
}

function adapterIsReady(state: AdapterStateSnapshot<string>): boolean {
  return state.availability === 'available' && state.power === 'on' && !isAuthorizationBlocking(state.authorization)
}

function assertAdapterCanBecomeReady(state: AdapterStateSnapshot<string>, operation: string): void {
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

async function nextAdapterState(
  iterator: import('../backend-contract/streams').BoundedAsyncStreamIterator<AdapterStateSnapshot<string>>,
  timeoutMs: number
): Promise<IteratorResult<import('../backend-contract/streams').StreamItem<AdapterStateSnapshot<string>>, undefined>> {
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

function snapshotPublicAdapterState(state: AdapterStateSnapshot<string>): BleAdapterState {
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
  source: BoundedAsyncStream<PublicScanObservation>,
  query: ReturnType<typeof normalizeScanQuery>
): BoundedAsyncStream<PublicScanObservation> {
  return {
    limits: source.limits,
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator](): BoundedAsyncStreamIterator<PublicScanObservation> {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next() {
          while (true) {
            const item = await iterator.next()
            if (item.done || item.value.kind !== 'value') return item
            if (observationMatchesScanQuery(query, normalizeScanObservation(item.value.value))) return item
          }
        },
        return: () => iterator.return(),
        [Symbol.asyncIterator]() {
          return this
        }
      }
    },
    close: () => source.close()
  }
}

export function peerFromPublicObservation(observation: PublicScanObservation): BlePeer {
  if (isCompactObservation(observation)) {
    return snapshotBlePeer({ id: observation.peerId, name: observation.localName, rssi: observation.rssi })
  }
  return snapshotBlePeer({
    id: String(observation.device.id),
    name: observation.localName.state === 'present' ? observation.localName.value : null,
    rssi: observation.rssi.state === 'present' ? observation.rssi.value : null
  })
}

function isCompactObservation(observation: PublicScanObservation): observation is IpcAdvertisement {
  return 'peerId' in observation
}

export function assertPublicScanOptions(options: ScanOptions): void {
  const allowed = new Set(['signal', 'timeoutMs', 'query', 'duplicates', 'delivery'])
  if (Object.keys(options).some(key => !allowed.has(key))) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.options')
  }
  if (options.duplicates !== undefined && options.duplicates !== 'coalesced' && options.duplicates !== 'all') {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.duplicates')
  }
  if (
    options.delivery !== undefined &&
    options.delivery !== 'latest' &&
    options.delivery !== 'balanced' &&
    options.delivery !== 'lossless-bounded' &&
    options.delivery !== 'custom'
  ) {
    throw contractError('argument.invalid', 'scan', 'public-ble-manager.scan.delivery')
  }
}

export async function findPeerInScan(scan: ScanSession, select: FindOptions['select']): Promise<BlePeer> {
  const iterator = scan.observations[Symbol.asyncIterator]()
  while (true) {
    const item = await iterator.next()
    if (item.done) throw rehydratePublicError(contractError('operation.timed-out', 'scan', 'public-ble-manager.find'))
    if (item.value.kind === 'terminal') {
      if (item.value.reason === 'operation-timed-out') {
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
