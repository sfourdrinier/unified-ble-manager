import { contractError } from '../backend-contract/errors'
import type {
  BackendPeerRecord,
  BackendPeerQuery,
  BlePeerState,
  PeerDirectoryBackend,
  PeerSource
} from '../backend-contract/backend'
import { canonicalUuid } from '../backend-contract/primitives'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import type { BlePeer } from './ble-manager'
import { assertPeerReference, encodePeerReference, snapshotPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'
import { rehydratePublicError } from './error-bridge'
import { normalizeScanObservation } from './scan-query'

export type { BlePeerState, PeerSource } from '../backend-contract/backend'

export interface KnownPeerQuery extends OperationOptions {
  readonly sources?: readonly PeerSource[]
  readonly services?: readonly (string | number)[]
  readonly references?: readonly PeerReference[]
  readonly includeUnavailable?: boolean
}

export interface BlePeerDirectory {
  resolve(reference: PeerReference, options?: OperationOptions): Promise<BlePeer | null>
  known(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  connected(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  bonded(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  authorized(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  restored(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
}

export function createPublicPeerDirectory(
  backend: PeerDirectoryBackend<string> | undefined,
  now: () => number
): BlePeerDirectory {
  if (backend === undefined) return unsupportedPeerDirectory()
  const invoke = async (
    operation: keyof Omit<PeerDirectoryBackend<string>, 'resolve'>,
    options: KnownPeerQuery = {}
  ): Promise<readonly BlePeer[]> => {
    try {
      const records = await backend[operation](toBackendPeerQuery(options, now))
      return mergePeerDirectoryRecords(records.map(toPublicPeerDirectoryRecord))
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
  return {
    resolve: async (reference, options = {}) => {
      try {
        assertPeerReference(reference, 'peer-directory.resolve')
        const record = await backend.resolve(reference, toBackendPeerQuery(options, now))
        if (record === null) return null
        return mergePeerDirectoryRecords([toPublicPeerDirectoryRecord(record)])[0] ?? null
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    known: options => invoke('known', options),
    connected: options => invoke('connected', options),
    bonded: options => invoke('bonded', options),
    authorized: options => invoke('authorized', options),
    restored: options => invoke('restored', options)
  }
}

export interface PeerDirectoryRecord {
  readonly reference: PeerReference
  readonly peer: BlePeer
  readonly source: PeerSource
  readonly state: BlePeerState
  readonly services?: readonly string[]
  readonly clockScope?: string
}

function toBackendPeerQuery(options: KnownPeerQuery, now: () => number): BackendPeerQuery {
  const normalized = normalizeOperationOptions(options, now)
  return {
    signal: normalized.signal,
    deadline: normalized.deadline,
    sources: options.sources,
    services: options.services?.map(value => canonicalUuid(typeof value === 'number' ? value.toString(16) : value)),
    references: options.references?.map((reference, index) => {
      assertPeerReference(reference, `peer-directory.references[${index}]`)
      return snapshotPeerReference(reference, `peer-directory.references[${index}]`)
    }),
    includeUnavailable: options.includeUnavailable
  }
}

function toPublicPeerDirectoryRecord(record: BackendPeerRecord<string>): PeerDirectoryRecord {
  const reference = snapshotPeerReference(record.reference, 'peer-directory.backend-reference')
  const state = Object.freeze({
    reachability: record.state.reachability,
    connection: record.state.connection,
    bond: record.state.bond,
    lastSeenAtMonotonicMs: record.state.lastSeenAtMonotonicMs
  })
  return {
    reference,
    peer: Object.freeze({
      id: String(record.peerId),
      name: record.name,
      rssi: record.rssi,
      reference,
      sources: Object.freeze([record.source]),
      lastAdvertisement: null,
      state
    }),
    source: record.source,
    state,
    clockScope: record.clockScope
  }
}

export function mergePeerDirectoryRecords(records: readonly PeerDirectoryRecord[]): readonly BlePeer[] {
  const merged = new Map<
    string,
    { peer: BlePeer; sources: Set<PeerSource>; state: BlePeerState; clockScope: string | null }
  >()
  for (const record of records) {
    assertPeerReference(record.reference, 'peer-directory.merge.reference')
    assertPeerDirectoryRecord(record)
    if (record.clockScope === undefined && record.state.lastSeenAtMonotonicMs !== null) {
      throw contractError('peer.reference-invalid', 'connection', 'peer-directory.clock-scope')
    }
  }
  const orderedRecords = [...records].sort(
    (left, right) =>
      compareCanonical(referenceKey(left.reference), referenceKey(right.reference)) ||
      sourcePriority(left.source) - sourcePriority(right.source) ||
      compareCanonical(left.source, right.source) ||
      compareCanonical(recordTieKey(left), recordTieKey(right))
  )
  for (const record of orderedRecords) {
    const key = referenceKey(record.reference)
    const state = snapshotPeerState(record.state)
    const current = merged.get(key)
    if (current === undefined) {
      merged.set(key, {
        peer: Object.freeze({
          ...record.peer,
          reference: Object.freeze({ ...record.reference }),
          sources: Object.freeze([record.source]),
          lastAdvertisement:
            record.peer.lastAdvertisement === undefined || record.peer.lastAdvertisement === null
              ? null
              : normalizeScanObservation(record.peer.lastAdvertisement),
          state
        }),
        sources: new Set([record.source]),
        state,
        clockScope: record.clockScope ?? null
      })
      continue
    }
    current.sources.add(record.source)
    const previousLastSeen = current.state.lastSeenAtMonotonicMs
    const sameClock = current.clockScope !== null && current.clockScope === (record.clockScope ?? null)
    if (!sameClock) current.clockScope = null
    current.state = mergePeerState(current.state, state, sameClock)
    const rightIsFresher =
      sameClock &&
      state.lastSeenAtMonotonicMs !== null &&
      (previousLastSeen === null || state.lastSeenAtMonotonicMs > previousLastSeen)
    current.peer = Object.freeze({
      ...current.peer,
      name: current.peer.name ?? record.peer.name,
      rssi: rightIsFresher ? (record.peer.rssi ?? current.peer.rssi) : current.peer.rssi,
      lastAdvertisement:
        rightIsFresher && record.peer.lastAdvertisement !== undefined && record.peer.lastAdvertisement !== null
          ? normalizeScanObservation(record.peer.lastAdvertisement)
          : current.peer.lastAdvertisement,
      sources: sourceOrder([...current.sources]),
      state: current.state
    })
  }
  return Object.freeze([...merged.values()].sort(compareMergedPeers).map(entry => entry.peer))
}

function snapshotPeerState(state: BlePeerState): BlePeerState {
  return Object.freeze({
    reachability: state.reachability,
    connection: state.connection,
    bond: state.bond,
    lastSeenAtMonotonicMs: state.lastSeenAtMonotonicMs
  })
}

function assertPeerDirectoryRecord(record: PeerDirectoryRecord): void {
  sourcePriority(record.source)
  const state = record.state
  if (
    typeof state !== 'object' ||
    state === null ||
    !['reachable', 'unreachable', 'unknown'].includes(state.reachability) ||
    !['connected', 'disconnected', 'unknown'].includes(state.connection) ||
    !['bonded', 'not-bonded', 'unknown', 'unsupported'].includes(state.bond) ||
    (state.lastSeenAtMonotonicMs !== null &&
      (!Number.isFinite(state.lastSeenAtMonotonicMs) || state.lastSeenAtMonotonicMs < 0))
  ) {
    throw contractError('peer.reference-invalid', 'connection', 'peer-directory.state')
  }
  if (
    typeof record.peer !== 'object' ||
    record.peer === null ||
    typeof record.peer.id !== 'string' ||
    record.peer.id.length === 0 ||
    !(typeof record.peer.name === 'string' || record.peer.name === null) ||
    !(record.peer.rssi === null || (typeof record.peer.rssi === 'number' && Number.isFinite(record.peer.rssi)))
  ) {
    throw contractError('peer.reference-invalid', 'connection', 'peer-directory.peer')
  }
  if (record.clockScope !== undefined && record.clockScope.length === 0) {
    throw contractError('peer.reference-invalid', 'connection', 'peer-directory.clock-scope')
  }
}

function recordTieKey(record: PeerDirectoryRecord): string {
  return JSON.stringify({
    clockScope: record.clockScope ?? null,
    id: record.peer.id,
    name: record.peer.name,
    rssi: record.peer.rssi,
    state: record.state
  })
}

function mergePeerState(left: BlePeerState, right: BlePeerState, sameClock: boolean): BlePeerState {
  return Object.freeze({
    reachability: strongest(left.reachability, right.reachability, ['unreachable', 'unknown', 'reachable']),
    connection: strongest(left.connection, right.connection, ['disconnected', 'unknown', 'connected']),
    bond: strongest(left.bond, right.bond, ['not-bonded', 'unknown', 'unsupported', 'bonded']),
    lastSeenAtMonotonicMs: sameClock
      ? left.lastSeenAtMonotonicMs === null
        ? right.lastSeenAtMonotonicMs
        : right.lastSeenAtMonotonicMs === null
          ? left.lastSeenAtMonotonicMs
          : Math.max(left.lastSeenAtMonotonicMs, right.lastSeenAtMonotonicMs)
      : null
  })
}

function strongest<Value extends string>(left: Value, right: Value, order: readonly Value[]): Value {
  return order.indexOf(left) >= order.indexOf(right) ? left : right
}

function compareCanonical(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}

function referenceKey(reference: PeerReference): string {
  return encodePeerReference(reference)
}

function sourceOrder(sources: readonly PeerSource[]): readonly PeerSource[] {
  return Object.freeze(
    [...sources].sort((left, right) => sourcePriority(left) - sourcePriority(right) || compareCanonical(left, right))
  )
}

function sourcePriority(source: PeerSource): number {
  const rank: Record<PeerSource, number> = {
    'system-connected': 0,
    restored: 1,
    'system-bonded': 2,
    'origin-authorized': 2,
    'app-reference': 3,
    'scan-observed': 4,
    'backend-cache': 5
  }
  const priority = rank[source]
  if (priority === undefined) {
    throw contractError('peer.reference-invalid', 'connection', 'peer-directory.source')
  }
  return priority
}

function compareMergedPeers(
  left: { readonly sources: ReadonlySet<PeerSource>; readonly peer: BlePeer },
  right: { readonly sources: ReadonlySet<PeerSource>; readonly peer: BlePeer }
): number {
  const leftSource = sourceOrder([...left.sources])[0] ?? 'backend-cache'
  const rightSource = sourceOrder([...right.sources])[0] ?? 'backend-cache'
  return (
    sourcePriority(leftSource) - sourcePriority(rightSource) ||
    compareCanonical(leftSource, rightSource) ||
    compareCanonical(left.peer.reference?.backendId ?? '', right.peer.reference?.backendId ?? '') ||
    compareCanonical(left.peer.reference?.opaqueId ?? left.peer.id, right.peer.reference?.opaqueId ?? right.peer.id)
  )
}

export function unsupportedPeerDirectory(): BlePeerDirectory {
  const unsupported = async (): Promise<readonly BlePeer[]> => {
    throw rehydratePublicError(contractError('capability.unsupported', 'connection', 'peer-directory'))
  }
  return {
    resolve: async () => {
      throw rehydratePublicError(contractError('capability.unsupported', 'connection', 'peer-directory.resolve'))
    },
    known: unsupported,
    connected: unsupported,
    bonded: unsupported,
    authorized: unsupported,
    restored: unsupported
  }
}
