import type { BackendPeerQuery, BackendPeerRecord, PeerDirectoryBackend, PeerSource } from '../backend-contract/backend'
import { contractError } from '../backend-contract/errors'
import { canonicalUuid, opaqueId } from '../backend-contract/primitives'
import type { SerializableRecord } from '../backend-contract/primitives'
import { snapshotPeerReference } from '../public/peer-reference'
import { createPublicPeerDirectory } from '../public/peer-directory'
import type { IpcBleManager } from './manager'

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value) || value instanceof Uint8Array) {
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.record')
  }
  return Object.fromEntries(Object.entries(value))
}

function text(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0)
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.text')
  return value
}

function source(value: unknown): PeerSource {
  if (
    value === 'scan-observed' ||
    value === 'app-reference' ||
    value === 'system-connected' ||
    value === 'system-bonded' ||
    value === 'origin-authorized' ||
    value === 'restored' ||
    value === 'backend-cache'
  )
    return value
  throw contractError('protocol.malformed', 'ipc', 'ipc.peers.source')
}

function array<Value>(value: unknown, decode: (entry: unknown) => Value): readonly Value[] {
  if (!Array.isArray(value)) throw contractError('protocol.malformed', 'ipc', 'ipc.peers.array')
  // Array.from visits sparse holes as undefined; map would silently preserve
  // them without applying the decoder to every transported entry.
  return Object.freeze(Array.from(value, decode))
}

/** Query fields are transported, never replaced with a cache or another query. */
export function decodePeerQuery(value: unknown): Omit<BackendPeerQuery, 'signal' | 'deadline'> {
  const query = record(value)
  if (Object.keys(query).some(key => !['services', 'sources', 'references', 'includeUnavailable'].includes(key)))
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.query-keys')
  if (query.includeUnavailable !== undefined && typeof query.includeUnavailable !== 'boolean')
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.include-unavailable')
  return {
    ...(query.services === undefined ? {} : { services: array(query.services, entry => canonicalUuid(text(entry))) }),
    ...(query.sources === undefined ? {} : { sources: array(query.sources, source) }),
    ...(query.references === undefined
      ? {}
      : { references: array(query.references, entry => snapshotPeerReference(entry, 'ipc.peers.reference')) }),
    ...(query.includeUnavailable === undefined ? {} : { includeUnavailable: query.includeUnavailable })
  }
}

export function encodePeerQuery(query: Omit<BackendPeerQuery, 'signal' | 'deadline'>): SerializableRecord {
  const checked = decodePeerQuery(query)
  return {
    ...(checked.services === undefined ? {} : { services: [...checked.services] }),
    ...(checked.sources === undefined ? {} : { sources: [...checked.sources] }),
    ...(checked.references === undefined
      ? {}
      : { references: checked.references.map(reference => ({ ...reference })) }),
    ...(checked.includeUnavailable === undefined ? {} : { includeUnavailable: checked.includeUnavailable })
  }
}

export function decodePeerRecord(value: unknown): BackendPeerRecord<string> {
  const peer = record(value),
    state = record(peer.state)
  const reference = snapshotPeerReference(peer.reference, 'ipc.peers.reference')
  if (
    !(peer.name === null || typeof peer.name === 'string') ||
    !(peer.rssi === null || (typeof peer.rssi === 'number' && Number.isFinite(peer.rssi)))
  )
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.observation')
  const reachability = state.reachability,
    connection = state.connection,
    bond = state.bond,
    lastSeenAtMonotonicMs = state.lastSeenAtMonotonicMs
  if (reachability !== 'reachable' && reachability !== 'unreachable' && reachability !== 'unknown')
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.reachability')
  if (connection !== 'connected' && connection !== 'disconnected' && connection !== 'unknown')
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.connection')
  if (bond !== 'bonded' && bond !== 'not-bonded' && bond !== 'unknown' && bond !== 'unsupported')
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.bond')
  if (
    !(
      lastSeenAtMonotonicMs === null ||
      (typeof lastSeenAtMonotonicMs === 'number' &&
        Number.isFinite(lastSeenAtMonotonicMs) &&
        lastSeenAtMonotonicMs >= 0)
    )
  )
    throw contractError('protocol.malformed', 'ipc', 'ipc.peers.timestamp')
  if (lastSeenAtMonotonicMs !== null && peer.clockScope === undefined)
    throw contractError('peer.reference-invalid', 'connection', 'ipc.peers.clock-scope')
  return {
    reference,
    peerId: opaqueId(text(peer.peerId), 'peer', 'ipc'),
    name: peer.name,
    rssi: peer.rssi,
    source: source(peer.source),
    state: { reachability, connection, bond, lastSeenAtMonotonicMs },
    ...(peer.clockScope === undefined ? {} : { clockScope: text(peer.clockScope) })
  }
}

export function encodePeerRecord(value: BackendPeerRecord<string>): SerializableRecord {
  const peer = decodePeerRecord(value)
  return {
    reference: { ...peer.reference },
    peerId: String(peer.peerId),
    name: peer.name,
    rssi: peer.rssi,
    source: peer.source,
    state: { ...peer.state },
    ...(peer.clockScope === undefined ? {} : { clockScope: peer.clockScope })
  }
}

/** Both desktop webview hosts use the existing authenticated, cancellable route. */
export function createIpcPeerDirectory(ipc: Pick<IpcBleManager<string, string>, 'route'>) {
  const list = async (category: Exclude<keyof PeerDirectoryBackend<string>, 'resolve'>, options: BackendPeerQuery) => {
    const { signal, deadline, ...query } = options
    const result = await ipc.route(
      `peers.${category}`,
      { query: encodePeerQuery(query), deadline: deadline ?? null },
      null,
      signal
    )
    return array(result.peers, decodePeerRecord)
  }
  const backend: PeerDirectoryBackend<string> = {
    resolve: async (reference, options) => {
      const result = await ipc.route(
        'peers.resolve',
        { reference: { ...reference }, deadline: options.deadline ?? null },
        null,
        options.signal
      )
      return result.peer === null ? null : decodePeerRecord(result.peer)
    },
    known: options => list('known', options),
    connected: options => list('connected', options),
    bonded: options => list('bonded', options),
    authorized: options => list('authorized', options),
    restored: options => list('restored', options)
  }
  return createPublicPeerDirectory(backend, () => globalThis.performance.now())
}
