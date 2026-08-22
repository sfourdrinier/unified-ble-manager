// src/public/ble-manager.ts — non-generic application façade (PR1 skeleton)

import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { ScanOptions as InternalScanOptions } from '../backend-contract/advertisement'
import type { ConnectionLifecycleCause, ConnectionLifecycleEvent } from '../backend-contract/connection-lifecycle'
import { contractError, type CleanupRecord } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import { capacity, canonicalUuid, opaqueId } from '../backend-contract/primitives'
import type { BleManager as InternalBleManager } from '../manager/ble-manager'
import type { BleManagerOptions } from '../manager/ble-manager'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { CoreBoundedStream } from '../core/bounded-stream'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import { resolveStreamPolicy } from './stream-presets'
import type { StreamPolicy } from './stream-presets'
import type { IpcAdvertisement } from '../ipc/manager'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from './error-bridge'
import { assertDirectConnectionCapability, PublicBleCapabilities } from './capabilities'
import type { BleCapabilities } from './capabilities'
import type { BleAdapter, BleAdapterState, AdapterReadinessOptions } from './ble-adapter'
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
import type { BoundedAsyncStreamIterator } from '../backend-contract/streams'
import { createScanState } from './scan-state'
import type { BlePeerDirectory, BlePeerState, PeerSource } from './peer-directory'
import { createPublicPeerDirectory } from './peer-directory'
import { encodePeerReference, isPeerReference, snapshotPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'
import type { ResourceCounters } from '../backend-contract/backend'
import { createPublicSecurity } from './security'
import type { BleSecurity } from './security'
import type { Limitation } from '../backend-contract/capabilities'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type ConnectionPriority,
  type ConnectionWriteReadinessWatch
} from '../backend-contract/connection-controls'

export type { ConnectionPriority } from '../backend-contract/connection-controls'

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

export interface PublicBleManagerHostOptions {
  readonly discoveryKind?: BleDiscoveryInfo['kind']
  readonly choose?: (options: ChooseOptions) => Promise<BlePeer>
  readonly peers?: BlePeerDirectory
}

type InternalPublicConnection = Awaited<ReturnType<InternalBleManager<string, BackendIdentity<string>>['connect']>>

interface OptionalInternalControlConnection {
  readonly effectiveMtu?: () => Promise<{
    readonly connectionId: string
    readonly connectionGeneration: string
    readonly attMtu: number | null
    readonly payloadBytes: number | null
    readonly platformPduBytes: number | null
    readonly observedAtMonotonicMs?: number
  }>
  readonly writeWithoutResponseReadiness?: () => Promise<ConnectionWriteReadinessWatch<string>>
}

type PublicControlConnection = InternalPublicConnection & OptionalInternalControlConnection

function controlMetadata(
  generation: string,
  now: number,
  descriptor: ReturnType<InternalBleManager<string, BackendIdentity<string>>['capability']>,
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

function requireControlCapability(
  internal: Pick<InternalBleManager<string, BackendIdentity<string>>, 'capability'>,
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

function publicWriteReadinessStream(
  connection: PublicControlConnection,
  generation: string,
  descriptor: ReturnType<InternalBleManager<string, BackendIdentity<string>>['capability']>
): AsyncIterable<WriteReadinessEvent> {
  return (async function* (): AsyncGenerator<WriteReadinessEvent> {
    const observe = connection.writeWithoutResponseReadiness
    if (observe === undefined) {
      throw contractError('capability.unsupported', 'connection', 'public-connection.controls.write-readiness')
    }
    const watch = await observe()
    try {
      for await (const item of watch.events) {
        if (item.kind === 'value') {
          yield Object.freeze({
            ...controlMetadata(generation, item.value.observedAtMonotonicMs, descriptor, 'backend-observation'),
            state: 'measured' as const,
            mode: 'without-response' as const,
            ready: item.value.ready
          })
        } else if (item.kind === 'overflow') {
          throw contractError('stream.overflow', 'connection', 'public-connection.controls.write-readiness')
        } else if (item.kind === 'terminal') {
          return
        }
      }
    } finally {
      await watch.close()
    }
  })()
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

function createPublicConnectionControls(
  internal: Pick<InternalBleManager<string, BackendIdentity<string>>, 'capability'>,
  connection: PublicControlConnection,
  generation: string,
  now: () => number
): BleConnectionControls {
  const readRssi = (options: OperationOptions = {}): Promise<RssiObservation> =>
    runPublicControl(async () => {
      const descriptor = requireControlCapability(internal, 'connection:rssi', 'public-connection.controls.read-rssi')
      const normalized = normalizeOperationOptions(options, now)
      const result = await connection.readRssi({ signal: normalized.signal, deadline: normalized.deadline })
      return Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'measured' as const,
        rssi: Number(result.rssi)
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
      const observation = Object.freeze({
        ...controlMetadata(generation, result.observedAtMonotonicMs, descriptor, 'backend-operation'),
        state: 'measured' as const,
        attMtu: Number(result.negotiatedMtu),
        payloadBytes: Number(result.negotiatedMtu) - 3,
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
    writeReadiness: (_mode: 'without-response') => {
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
  readonly peers: BlePeerDirectory
  readonly security: BleSecurity

  constructor(
    private readonly internal: InternalBleManager<string, BackendIdentity<string>>,
    private readonly now: () => number,
    hostOptions: PublicBleManagerHostOptions
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
            scanState.emit(
              cleanup.state === 'released' ? { state: 'stopped' } : { state: 'failed', reason: 'scan-stop-failed' }
            )
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
      const peerId = opaqueId<'peer', string>(peerIdString, 'peer', 'public-ble-manager')
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

function publicTraceDocument(
  internal: InternalBleManager<string, BackendIdentity<string>>
): ReturnType<InternalBleManager<string, BackendIdentity<string>>['traceDocument']> {
  return internal.attachedBackend?.backend.traceDocument?.() ?? internal.traceDocument()
}

function resolveSecurityBackend(
  internal: InternalBleManager<string, BackendIdentity<string>>
): import('../backend-contract/security').SecurityBackend | undefined {
  if (typeof internal.securityBackend === 'function') return internal.securityBackend()
  return internal.attachedBackend?.backend?.security
}

function publicResourceCounters(
  internal: InternalBleManager<string, BackendIdentity<string>>
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
  if (cleanup.state === 'release-failed') failures.push(new Error('BLE adapter watch cleanup failed'))
  if (failures.length > 0) throw new AggregateError(failures, 'BLE adapter watch cleanup failed')
  return cleanup
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
  source: BoundedAsyncStream<AdvertisementObservation<string> | IpcAdvertisement>,
  query: ReturnType<typeof normalizeScanQuery>,
  duplicates: 'coalesced' | 'all' = 'all'
): BoundedAsyncStream<PublicScanObservation> {
  const lastObservations = new Map<string, string>()
  return {
    limits: source.limits,
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator](): BoundedAsyncStreamIterator<PublicScanObservation> {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next() {
          while (true) {
            const item = await iterator.next()
            if (item.done) return item
            if (item.value.kind === 'overflow' || item.value.kind === 'terminal') {
              return { done: false, value: item.value }
            }
            const observation = projectPublicScanObservation(item.value.value)
            if (observationMatchesScanQuery(query, observation)) {
              if (duplicates === 'coalesced') {
                const fingerprint = publicObservationFingerprint(observation)
                if (lastObservations.get(observation.peer.id) === fingerprint) continue
                lastObservations.set(observation.peer.id, fingerprint)
              }
              return { done: false, value: { kind: 'value', value: observation } }
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
        for (const subscriber of this.subscribers) subscriber.emit(event, 512)
      }
      this.terminalReason = 'closed'
      for (const subscriber of this.subscribers) subscriber.closeWithReason('closed')
    } catch {
      this.terminalReason = 'source-failed'
      for (const subscriber of this.subscribers) subscriber.closeWithReason('source-failed')
    }
  }
}

export function peerFromPublicObservation(
  observation: PublicScanObservation | AdvertisementObservation<string> | IpcAdvertisement
): BlePeer {
  return 'peer' in observation ? observation.peer : projectPublicScanObservation(observation).peer
}

function projectPublicScanObservation(
  observation: AdvertisementObservation<string> | IpcAdvertisement
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
  const allowed = new Set(['signal', 'timeoutMs', 'query', 'duplicates', 'delivery'])
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
}

export function assertPublicConnectOptions(options: ConnectOptions): void {
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
