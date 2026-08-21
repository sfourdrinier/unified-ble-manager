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
  const merged = new Map<string, { peer: BlePeer; sources: Set<PeerSource>; state: BlePeerState }>()
  for (const record of records) {
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
        state: record.state
      })
      continue
    }
    current.sources.add(record.source)
    current.state = mergePeerState(current.state, record.state)
    current.peer = Object.freeze({
      ...current.peer,
      name: current.peer.name ?? record.peer.name,
      rssi: record.peer.rssi ?? current.peer.rssi,
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

function mergePeerState(left: BlePeerState, right: BlePeerState): BlePeerState {
  return Object.freeze({
    reachability:
      left.reachability === 'reachable' || right.reachability === 'reachable' ? 'reachable' : left.reachability,
    connection: left.connection === 'connected' || right.connection === 'connected' ? 'connected' : left.connection,
    bond: left.bond === 'bonded' || right.bond === 'bonded' ? 'bonded' : left.bond,
    lastSeenAtMonotonicMs:
      left.lastSeenAtMonotonicMs === null
        ? right.lastSeenAtMonotonicMs
        : right.lastSeenAtMonotonicMs === null
          ? left.lastSeenAtMonotonicMs
          : Math.max(left.lastSeenAtMonotonicMs, right.lastSeenAtMonotonicMs)
  })
}

function referenceKey(reference: PeerReference): string {
  return `${reference.version}|${reference.backendId}|${reference.scope}|${reference.opaqueId}`
}

function sourceOrder(sources: readonly PeerSource[]): readonly PeerSource[] {
  const rank: Record<PeerSource, number> = {
    'system-connected': 0,
    restored: 1,
    'system-bonded': 2,
    'origin-authorized': 2,
    'app-reference': 3,
    'scan-observed': 4,
    'backend-cache': 5
  }
  return Object.freeze([...sources].sort((left, right) => rank[left] - rank[right] || left.localeCompare(right)))
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
    resolve: async reference => {
      try {
        assertPeerReference(reference, 'peer-directory.resolve')
      } catch (error) {
        throw rehydratePublicError(error)
      }
      return null
    },
    known: unsupported,
    connected: unsupported,
    bonded: unsupported,
    authorized: unsupported,
    restored: unsupported
  }
}

export function createPeerDirectory(backendId: string | null): BlePeerDirectory {
  const unsupported = unsupportedPeerDirectory()
  return {
    ...unsupported,
    resolve: async reference => {
      try {
        assertPeerReference(reference, 'peer-directory.resolve')
        if (backendId === null || reference.backendId !== backendId) {
          throw contractError('peer.scope-mismatch', 'connection', 'peer-directory.resolve-backend')
        }
        const source: PeerSource = 'app-reference'
        const state: BlePeerState = {
          reachability: 'unknown',
          connection: 'unknown',
          bond: 'unknown',
          lastSeenAtMonotonicMs: null
        }
        return Object.freeze({
          id: reference.opaqueId,
          name: null,
          rssi: null,
          reference: Object.freeze({ ...reference }),
          sources: Object.freeze([source]),
          state: Object.freeze(state)
        })
      } catch (error) {
        throw rehydratePublicError(error)
      }
    }
  }
}
