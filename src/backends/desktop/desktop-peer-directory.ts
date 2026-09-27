import type {
  BackendPeerQuery,
  BackendPeerRecord,
  PeerDirectoryBackend,
  PeerSource
} from '../../backend-contract/backend'
import { contractError } from '../../backend-contract/errors'
import { assertPeerReference, type PeerReference } from '../../backend-contract/peer-reference'
import { canonicalUuid, type PeerId } from '../../backend-contract/primitives'
import type { DesktopRustCoreDirectoryPeer } from './desktop-rust-core-binding'

/** Read-only OS retrieval. These hooks never acquire a connection lease. */
export interface DesktopPeerDirectoryHooks {
  readonly backendId: string
  generation(): number
  connected(services: readonly string[], options: BackendPeerQuery): Promise<readonly DesktopRustCoreDirectoryPeer[]>
  resolve(peerId: string, options: BackendPeerQuery): Promise<DesktopRustCoreDirectoryPeer | null>
  peerId(nativeId: string): PeerId<string>
  assertUsable(operation: string, options: BackendPeerQuery): void
}

const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/iu

/** CoreBluetooth's directory semantics, independent of connection ownership. */
export function createDesktopPeerDirectory(hooks: DesktopPeerDirectoryHooks): PeerDirectoryBackend<string> {
  const admit = (operation: string, options: BackendPeerQuery) => {
    hooks.assertUsable(operation, options)
    const generation = hooks.generation()
    return () => {
      hooks.assertUsable(operation, options)
      if (hooks.generation() !== generation) throw contractError('lifecycle.invalid-state', 'core', operation)
    }
  }
  const unsupported = (operation: string): never => {
    throw contractError('capability.unsupported', 'connection', operation)
  }
  const identifier = (reference: PeerReference, operation: string): string => {
    assertPeerReference(reference, operation)
    if (reference.backendId !== hooks.backendId || reference.scope !== 'application') {
      throw contractError('peer.scope-mismatch', 'connection', operation)
    }
    if (!GUID.test(reference.opaqueId)) throw contractError('peer.reference-invalid', 'connection', operation)
    return reference.opaqueId.toLowerCase()
  }
  const validateRecord = (record: DesktopRustCoreDirectoryPeer, operation: string): void => {
    if (
      record === null ||
      typeof record !== 'object' ||
      typeof record.peerId !== 'string' ||
      !GUID.test(record.peerId) ||
      !(record.name === null || typeof record.name === 'string') ||
      !['connected', 'disconnected', 'unknown'].includes(record.connection)
    )
      throw contractError('protocol.malformed', 'platform', operation)
  }
  const map = (record: DesktopRustCoreDirectoryPeer, source: PeerSource): BackendPeerRecord<string> => {
    const nativeId = record.peerId.toLowerCase()
    return Object.freeze({
      reference: Object.freeze({ version: 1, backendId: hooks.backendId, scope: 'application', opaqueId: nativeId }),
      peerId: hooks.peerId(nativeId),
      name: record.name,
      rssi: null,
      source,
      state: Object.freeze({
        reachability: 'unknown',
        connection: record.connection,
        bond: 'unsupported',
        lastSeenAtMonotonicMs: null
      })
    })
  }
  const resolve = async (
    reference: PeerReference,
    options: BackendPeerQuery
  ): Promise<BackendPeerRecord<string> | null> => {
    const operation = 'peers.resolve'
    const assertCurrent = admit(operation, options)
    const nativeId = identifier(reference, operation)
    if (options.services !== undefined && options.services.length > 0) unsupported(`${operation}.services`)
    const record = await hooks.resolve(nativeId, options)
    assertCurrent()
    if (record === null) return null
    validateRecord(record, operation)
    if (record.peerId.toLowerCase() !== nativeId) throw contractError('protocol.malformed', 'platform', operation)
    if (options.sources !== undefined && !options.sources.includes('app-reference')) return null
    return map(record, 'app-reference')
  }
  return Object.freeze({
    resolve,
    known: async (options: BackendPeerQuery) => {
      const operation = 'peers.known'
      const assertCurrent = admit(operation, options)
      if (options.references === undefined) return unsupported(`${operation}.references-required`)
      if (options.services !== undefined && options.services.length > 0) return unsupported(`${operation}.services`)
      // Validate every reference before the first native query, then deduplicate.
      const references = new Map(options.references.map(reference => [identifier(reference, operation), reference]))
      const records: BackendPeerRecord<string>[] = []
      for (const reference of references.values()) {
        const record = await resolve(reference, options)
        assertCurrent()
        if (record !== null) records.push(record)
      }
      return Object.freeze(records)
    },
    connected: async (options: BackendPeerQuery) => {
      const operation = 'peers.connected'
      const assertCurrent = admit(operation, options)
      if (options.services === undefined || options.services.length === 0)
        return unsupported(`${operation}.services-required`)
      const services = [...new Set(options.services.map(canonicalUuid))]
      const references =
        options.references === undefined ? null : new Set(options.references.map(ref => identifier(ref, operation)))
      const records = await hooks.connected(services, options)
      assertCurrent()
      if (!Array.isArray(records)) throw contractError('protocol.malformed', 'platform', operation)
      for (const record of records) {
        validateRecord(record, operation)
        if (record.connection !== 'connected') throw contractError('protocol.malformed', 'platform', operation)
      }
      return Object.freeze(
        records
          .filter(record => references === null || references.has(record.peerId.toLowerCase()))
          .filter(() => options.sources === undefined || options.sources.includes('system-connected'))
          .map(record => map(record, 'system-connected'))
      )
    },
    bonded: async () => unsupported('peers.bonded'),
    authorized: async () => unsupported('peers.authorized'),
    restored: async () => unsupported('peers.restored')
  })
}
