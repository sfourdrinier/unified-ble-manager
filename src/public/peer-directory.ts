import { contractError } from '../backend-contract/errors'
import type { OperationOptions } from './operation-options'
import type { BlePeer } from './ble-manager'
import { assertPeerReference } from './peer-reference'
import type { PeerReference } from './peer-reference'
import { rehydratePublicError } from './error-bridge'

export type PeerSource =
  | 'scan-observed'
  | 'app-reference'
  | 'system-connected'
  | 'system-bonded'
  | 'origin-authorized'
  | 'restored'
  | 'backend-cache'

export interface KnownPeerQuery extends OperationOptions {
  readonly sources?: readonly PeerSource[]
  readonly services?: readonly (string | number)[]
  readonly references?: readonly PeerReference[]
  readonly includeUnavailable?: boolean
}

export interface BlePeerState {
  readonly reachability: 'reachable' | 'unreachable' | 'unknown'
  readonly connection: 'connected' | 'disconnected' | 'unknown'
  readonly bond: 'bonded' | 'not-bonded' | 'unknown' | 'unsupported'
  readonly lastSeenAtMonotonicMs: number | null
}

export interface BlePeerDirectory {
  resolve(reference: PeerReference, options?: OperationOptions): Promise<BlePeer | null>
  known(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  connected(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  bonded(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  authorized(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  restored(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
}

export interface PeerDirectoryRecord {
  readonly reference: PeerReference
  readonly peer: BlePeer
  readonly source: PeerSource
  readonly state: BlePeerState
  readonly services?: readonly string[]
  readonly clockScope?: string
}

export function mergePeerDirectoryRecords(records: readonly PeerDirectoryRecord[]): readonly BlePeer[] {
  const merged = new Map<
    string,
    { peer: BlePeer; sources: Set<PeerSource>; state: BlePeerState; clockScope: string | null }
  >()
  const orderedRecords = [...records].sort(
    (left, right) =>
      referenceKey(left.reference).localeCompare(referenceKey(right.reference)) ||
      sourcePriority(left.source) - sourcePriority(right.source) ||
      left.source.localeCompare(right.source)
  )
  for (const record of orderedRecords) {
    assertPeerReference(record.reference, 'peer-directory.merge.reference')
    if (record.clockScope === undefined && record.state.lastSeenAtMonotonicMs !== null) {
      throw contractError('peer.reference-invalid', 'connection', 'peer-directory.clock-scope')
    }
    const key = referenceKey(record.reference)
    const current = merged.get(key)
    if (current === undefined) {
      merged.set(key, {
        peer: Object.freeze({
          ...record.peer,
          reference: Object.freeze({ ...record.reference }),
          sources: [record.source],
          state: record.state
        }),
        sources: new Set([record.source]),
        state: record.state,
        clockScope: record.clockScope ?? null
      })
      continue
    }
    current.sources.add(record.source)
    const previousLastSeen = current.state.lastSeenAtMonotonicMs
    const sameClock = current.clockScope !== null && current.clockScope === (record.clockScope ?? null)
    if (!sameClock) current.clockScope = null
    current.state = mergePeerState(current.state, record.state, sameClock)
    const rightIsFresher =
      sameClock &&
      record.state.lastSeenAtMonotonicMs !== null &&
      (previousLastSeen === null || record.state.lastSeenAtMonotonicMs > previousLastSeen)
    current.peer = Object.freeze({
      ...current.peer,
      name: current.peer.name ?? record.peer.name,
      rssi: rightIsFresher ? (record.peer.rssi ?? current.peer.rssi) : current.peer.rssi,
      sources: sourceOrder([...current.sources]),
      state: current.state
    })
  }
  return Object.freeze(
    [...merged.values()]
      .sort((left, right) => peerSortKey(left.sources, left.peer).localeCompare(peerSortKey(right.sources, right.peer)))
      .map(entry => entry.peer)
  )
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

function referenceKey(reference: PeerReference): string {
  return `${reference.version}|${reference.backendId}|${reference.scope}|${reference.opaqueId}`
}

function sourceOrder(sources: readonly PeerSource[]): readonly PeerSource[] {
  return Object.freeze(
    [...sources].sort((left, right) => sourcePriority(left) - sourcePriority(right) || left.localeCompare(right))
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
  return rank[source]
}

function peerSortKey(sources: ReadonlySet<PeerSource>, peer: BlePeer): string {
  const ordered = sourceOrder([...sources])
  return `${String(ordered[0] ?? 'backend-cache')}|${peer.reference?.backendId ?? ''}|${peer.reference?.opaqueId ?? peer.id}`
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
