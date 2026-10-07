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
  known?(options: BackendPeerQuery): Promise<readonly DesktopRustCoreDirectoryPeer[]>
  connected(services: readonly string[], options: BackendPeerQuery): Promise<readonly DesktopRustCoreDirectoryPeer[]>
  resolve(peerId: string, options: BackendPeerQuery): Promise<DesktopRustCoreDirectoryPeer | null>
  bonded?(options: BackendPeerQuery): Promise<readonly DesktopRustCoreDirectoryPeer[]>
  readonly resolveFromBonded?: boolean
  peerId(nativeId: string): PeerId<string>
  assertUsable(operation: string, options: BackendPeerQuery): void
}

const GUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/iu
const ADDRESS = /^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$/iu
const BLUEZ = /^hci[0-9]+\/dev_(?:[0-9A-F]{2}_){5}[0-9A-F]{2}$/u

/** Native OS directory semantics, independent of connection ownership. */
export function createDesktopPeerDirectory(hooks: DesktopPeerDirectoryHooks): PeerDirectoryBackend<string> {
  const nativeIdentifier = (value: string): string | null => {
    if (hooks.backendId === 'unified-ble:winrt') {
      const split = /^(public|random|unknown):(.+)$/u.exec(value)
      if (split !== null) return ADDRESS.test(split[2] ?? '') ? `${split[1]}:${split[2]?.toUpperCase()}` : null
      return ADDRESS.test(value) ? value.toUpperCase() : null
    }
    if (hooks.backendId === 'unified-ble:bluez-dbus') return BLUEZ.test(value) ? value : null
    return GUID.test(value) ? value.toLowerCase() : null
  }
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
    const id = nativeIdentifier(reference.opaqueId)
    if (id === null) throw contractError('peer.reference-invalid', 'connection', operation)
    return id
  }
  const validateRecord = (record: DesktopRustCoreDirectoryPeer, operation: string): void => {
    if (
      record === null ||
      typeof record !== 'object' ||
      typeof record.peerId !== 'string' ||
      nativeIdentifier(record.peerId) === null ||
      !(record.name === null || typeof record.name === 'string') ||
      !['connected', 'disconnected', 'unknown'].includes(record.connection)
    )
      throw contractError('protocol.malformed', 'platform', operation)
  }
  const map = (record: DesktopRustCoreDirectoryPeer, source: PeerSource): BackendPeerRecord<string> => {
    const nativeId = nativeIdentifier(record.peerId)
    if (nativeId === null) throw contractError('protocol.malformed', 'platform', 'peers.record')
    return Object.freeze({
      reference: Object.freeze({ version: 1, backendId: hooks.backendId, scope: 'application', opaqueId: nativeId }),
      peerId: hooks.peerId(nativeId),
      name: record.name,
      rssi: null,
      source,
      state: Object.freeze({
        reachability: 'unknown',
        connection: record.connection,
        bond:
          source === 'system-bonded'
            ? 'bonded'
            : hooks.backendId === 'unified-ble:corebluetooth'
              ? 'unsupported'
              : 'unknown',
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
    const source: PeerSource = hooks.resolveFromBonded === true ? 'system-bonded' : 'app-reference'
    let record: DesktopRustCoreDirectoryPeer | null
    if (hooks.resolveFromBonded === true) {
      if (hooks.bonded === undefined) return unsupported(operation)
      const records = await hooks.bonded(options)
      assertCurrent()
      if (!Array.isArray(records)) throw contractError('protocol.malformed', 'platform', operation)
      for (const candidate of records) validateRecord(candidate, operation)
      record = records.find(candidate => nativeIdentifier(candidate.peerId) === nativeId) ?? null
    } else record = await hooks.resolve(nativeId, options)
    assertCurrent()
    if (record === null) return null
    validateRecord(record, operation)
    if (nativeIdentifier(record.peerId) !== nativeId) throw contractError('protocol.malformed', 'platform', operation)
    if (options.sources !== undefined && !options.sources.includes(source)) return null
    return map(record, source)
  }
  return Object.freeze({
    resolve,
    known: async (options: BackendPeerQuery) => {
      const operation = 'peers.known'
      const assertCurrent = admit(operation, options)
      if (hooks.known !== undefined) {
        if (options.services !== undefined && options.services.length > 0) return unsupported(`${operation}.services`)
        const references =
          options.references === undefined ? null : new Set(options.references.map(ref => identifier(ref, operation)))
        const records = await hooks.known(options)
        assertCurrent()
        if (!Array.isArray(records)) throw contractError('protocol.malformed', 'platform', operation)
        for (const record of records) validateRecord(record, operation)
        return Object.freeze(
          records
            .filter(record => references === null || references.has(nativeIdentifier(record.peerId) ?? ''))
            .filter(() => options.sources === undefined || options.sources.includes('backend-cache'))
            .map(record => map(record, 'backend-cache'))
        )
      }
      if (hooks.resolveFromBonded === true) return unsupported(operation)
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
      const apple = hooks.backendId === 'unified-ble:corebluetooth'
      if (apple && (options.services === undefined || options.services.length === 0))
        return unsupported(`${operation}.services-required`)
      if (!apple && options.services !== undefined && options.services.length > 0)
        return unsupported(`${operation}.services`)
      const services = [...new Set((options.services ?? []).map(canonicalUuid))]
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
          .filter(record => references === null || references.has(nativeIdentifier(record.peerId) ?? ''))
          .filter(() => options.sources === undefined || options.sources.includes('system-connected'))
          .map(record => map(record, 'system-connected'))
      )
    },
    bonded: async (options: BackendPeerQuery) => {
      const operation = 'peers.bonded'
      const assertCurrent = admit(operation, options)
      if (hooks.bonded === undefined) return unsupported(operation)
      if (options.services !== undefined && options.services.length > 0) return unsupported(`${operation}.services`)
      const references =
        options.references === undefined ? null : new Set(options.references.map(ref => identifier(ref, operation)))
      const records = await hooks.bonded(options)
      assertCurrent()
      if (!Array.isArray(records)) throw contractError('protocol.malformed', 'platform', operation)
      for (const record of records) validateRecord(record, operation)
      return Object.freeze(
        records
          .filter(record => references === null || references.has(nativeIdentifier(record.peerId) ?? ''))
          .filter(() => options.sources === undefined || options.sources.includes('system-bonded'))
          .map(record => map(record, 'system-bonded'))
      )
    },
    authorized: async () => unsupported('peers.authorized'),
    restored: async () => unsupported('peers.restored')
  })
}
