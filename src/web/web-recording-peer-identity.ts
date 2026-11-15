import { webIndexedDbQueueAbortError } from './web-recording-errors'
import type { PeerReference } from '../backend-contract/peer-reference'
import { snapshotPeerReference } from '../backend-contract/peer-reference'
import { isRecordingId } from '../backend-contract/continuation-recording-bounds'
import { webRecordingContractError as contractError } from './web-recording-errors'
import { runWebIndexedDbTransaction } from './web-indexeddb-transaction'
import { webRecordingCanonical } from './web-ordered-recording-journal'
import type { WebOrderedJournalDatabase } from './web-ordered-recording-journal'
import type { WebAuthorityOwner } from './web-authority-reservation'

/** Browser-origin peer reference returned by the Web chooser. */
export type WebRecordingPeerReference = PeerReference & { readonly scope: 'origin' }

/** Persistent host integration boundary for recovering a recording's browser
 * peer. The shared ordered-recording context contains only an ephemeral peerId.
 *
 * Bind must atomically install the complete reference if the recording ID is
 * unbound, preserve an existing reference, and compare all reference fields.
 * A successful result means the binding is durable and visible to reads and
 * other contexts in the same browser storage partition. On an indeterminate
 * outcome it must reject; callers must reconcile by reading before retrying.
 * The host must retain bindings for as long as the recording can be recovered.
 * This contract does not itself enable a Web recording or recovery route. */
export interface WebRecordingPeerIdentityStore {
  bind(
    recordingId: string,
    reference: WebRecordingPeerReference
  ): Promise<{ readonly state: 'stored' | 'already-bound' | 'conflict' }>
  /** Bind only while the durable reservation and producer declaration still match. */
  bindOwned?(
    owner: WebAuthorityOwner,
    producerIncarnation: string,
    reference: WebRecordingPeerReference
  ): Promise<{ readonly state: 'stored' | 'already-bound' | 'conflict' }>
  readOwned?(owner: WebAuthorityOwner, producerIncarnation: string): Promise<WebRecordingPeerReference | null>
  /** Returns null only when absence is established; an uncertain read rejects. */
  read(recordingId: string): Promise<WebRecordingPeerReference | null>
}

/** The reserved key belongs to the same transaction domain as reader receipts. */
export function webRecordingPeerKey(id: string): string {
  if (!isRecordingId(id)) throw contractError('argument.invalid', 'restoration', 'web-recording.peer-id')
  return `:ubm-peer-v1:${id}`
}

/** Parse both retained legacy rows and incarnation-scoped rows without upgrading either. */
export function parseWebRecordingPeerRow(
  value: unknown,
  expectedKey: string
):
  | { readonly version: 1; readonly reference: WebRecordingPeerReference }
  | {
      readonly version: 2
      readonly producerIncarnation: string
      readonly ownerEpoch: string
      readonly reference: WebRecordingPeerReference
    }
  | null {
  if (value === undefined) return null
  if (
    typeof value !== 'object' ||
    value === null ||
    Array.isArray(value) ||
    Reflect.get(value, 'recordingId') !== expectedKey ||
    !Object.hasOwn(value, 'reference')
  )
    throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-row')
  const raw = snapshotPeerReference(Reflect.get(value, 'reference'), 'web-recording.peer-reference')
  if (raw.scope !== 'origin')
    throw contractError('peer.reference-invalid', 'restoration', 'web-recording.peer-reference')
  const reference = { ...raw, scope: 'origin' as const }
  if (Reflect.get(value, 'version') === 1 && Object.keys(value).length === 3) return { version: 1, reference }
  const producerIncarnation = Reflect.get(value, 'producerIncarnation')
  const ownerEpoch = Reflect.get(value, 'ownerEpoch')
  if (
    Reflect.get(value, 'version') === 2 &&
    Object.keys(value).length === 5 &&
    typeof producerIncarnation === 'string' &&
    producerIncarnation.length > 0 &&
    producerIncarnation.length <= 256 &&
    typeof ownerEpoch === 'string' &&
    /^[1-9][0-9]*$/.test(ownerEpoch) &&
    ownerEpoch.length <= 20 &&
    BigInt(ownerEpoch) <= 18446744073709551615n
  )
    return { version: 2, producerIncarnation, ownerEpoch, reference }
  throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-row')
}

function validAcquisitionReceipt(value: unknown): boolean {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const operationId = Reflect.get(value, 'operationId')
  return (
    typeof operationId === 'string' &&
    /^[0-9a-f]{64}$/.test(operationId) &&
    Reflect.get(value, 'transition') === 'acquire' &&
    Reflect.get(value, 'phase') === 'provisional'
  )
}

/** Store peer bindings in the partition's journal database. Reserved keys are
 * outside the public recording ID alphabet and cannot alias reader receipts. */
export function createWebRecordingPeerIdentityStore(
  database: WebOrderedJournalDatabase
): WebRecordingPeerIdentityStore {
  const reference = (value: unknown): WebRecordingPeerReference => {
    const result = snapshotPeerReference(value, 'web-recording.peer-reference')
    if (result.scope !== 'origin')
      throw contractError('peer.reference-invalid', 'restoration', 'web-recording.peer-reference')
    return { ...result, scope: 'origin' }
  }
  const readRow = (value: unknown, expectedKey: string): WebRecordingPeerReference | null =>
    parseWebRecordingPeerRow(value, expectedKey)?.reference ?? null
  const transact = async <T>(
    id: string,
    mode: 'readonly' | 'readwrite',
    action: (
      existing: WebRecordingPeerReference | null,
      store: ReturnType<ReturnType<WebOrderedJournalDatabase['transaction']>['objectStore']>,
      key: string
    ) => T
  ): Promise<T> => {
    const rowKey = webRecordingPeerKey(id)
    let result: T | undefined
    let problem: unknown
    try {
      await runWebIndexedDbTransaction(database, ['readerReceipts'], mode, tx => {
        const store = tx.objectStore('readerReceipts')
        const request = store.get(rowKey)
        request.onsuccess = () => {
          try {
            result = action(readRow(request.result, rowKey), store, rowKey)
          } catch (error) {
            problem = error
            try {
              tx.abort()
            } catch (abortError) {
              problem = webIndexedDbQueueAbortError(error, abortError)
            }
          }
        }
      })
    } catch (error) {
      throw problem ?? error
    }
    if (problem !== undefined) throw problem
    if (result === undefined) throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-transaction')
    return result
  }
  const owned = async (
    owner: WebAuthorityOwner,
    producerIncarnation: string,
    peer: WebRecordingPeerReference | null
  ): Promise<{ readonly state: 'stored' | 'already-bound' | 'conflict' } | WebRecordingPeerReference | null> => {
    const key = webRecordingPeerKey(owner.id)
    const expected = { ...owner }
    if (
      !/^[0-9a-f]{64}$/.test(expected.ownerToken) ||
      !/^[1-9][0-9]*$/.test(expected.generation) ||
      !/^[1-9][0-9]*$/.test(expected.ownerEpoch) ||
      !producerIncarnation
    )
      throw contractError('argument.invalid', 'restoration', 'web-recording.peer-owner')
    let result:
      | { value: { readonly state: 'stored' | 'already-bound' | 'conflict' } | WebRecordingPeerReference | null }
      | undefined
    let problem: unknown
    try {
      await runWebIndexedDbTransaction(
        database,
        ['recordings', 'authorityReservations', 'readerReceipts'],
        peer === null ? 'readonly' : 'readwrite',
        tx => {
          const guard = (fn: () => void): void => {
            try {
              fn()
            } catch (error) {
              problem = error
              try {
                tx.abort()
              } catch (abortError) {
                problem = webIndexedDbQueueAbortError(error, abortError)
              }
            }
          }
          const policy = tx.objectStore('readerReceipts').get(':ubm-authority-policy-v1')
          const journal = tx.objectStore('recordings').get(expected.id)
          journal.onsuccess = () =>
            guard(() => {
              const row = journal.result
              const reservation = tx.objectStore('authorityReservations').get(expected.id)
              reservation.onsuccess = () =>
                guard(() => {
                  const durable = reservation.result
                  const authorityPolicy = policy.result
                  if (
                    typeof authorityPolicy !== 'object' ||
                    authorityPolicy === null ||
                    Reflect.get(authorityPolicy, 'recordingId') !== ':ubm-authority-policy-v1' ||
                    Reflect.get(authorityPolicy, 'version') !== 2 ||
                    Reflect.get(authorityPolicy, 'protocol') !== 'ubm-authority-reservations/1' ||
                    typeof row !== 'object' ||
                    row === null ||
                    typeof durable !== 'object' ||
                    durable === null ||
                    Reflect.get(row, 'id') !== expected.id ||
                    Reflect.get(row, 'version') !== 1 ||
                    Reflect.get(row, 'kind') !== 'ordered' ||
                    Reflect.get(row, 'ownerState') !== 'prepared' ||
                    Reflect.get(row, 'producerIncarnation') !== producerIncarnation ||
                    Reflect.get(row, 'ownerEpoch') !== expected.ownerEpoch ||
                    Reflect.get(durable, 'id') !== expected.id ||
                    Reflect.get(durable, 'version') !== 1 ||
                    Reflect.get(durable, 'generation') !== expected.generation ||
                    Reflect.get(durable, 'ownerToken') !== expected.ownerToken ||
                    Reflect.get(durable, 'ownerEpoch') !== expected.ownerEpoch ||
                    Reflect.get(durable, 'phase') !== 'provisional' ||
                    !validAcquisitionReceipt(Reflect.get(durable, 'receipt')) ||
                    Reflect.get(durable, 'declaration') !==
                      webRecordingCanonical(
                        [
                          'id',
                          'maxBytes',
                          'maxRecords',
                          'context',
                          'producerIncarnation',
                          'collectionId',
                          'sourceGeneration',
                          'memberEpochs'
                        ].map(field => Reflect.get(row, field))
                      )
                  )
                    throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.peer-authority')
                  const store = tx.objectStore('readerReceipts')
                  const request = store.get(key)
                  request.onsuccess = () =>
                    guard(() => {
                      const existing = parseWebRecordingPeerRow(request.result, key)
                      if (
                        existing !== null &&
                        (existing.version !== 2 ||
                          existing.producerIncarnation !== producerIncarnation ||
                          existing.ownerEpoch !== expected.ownerEpoch)
                      ) {
                        // A later provisional reservation can only replace an untouched,
                        // revoked prepared journal. The previous epoch may be rebound
                        // only to the identical peer, never to a legacy or newer row.
                        if (
                          peer !== null &&
                          existing.version === 2 &&
                          BigInt(existing.ownerEpoch) < BigInt(expected.ownerEpoch) &&
                          webRecordingCanonical(existing.reference) === webRecordingCanonical(peer)
                        ) {
                          store.put({
                            recordingId: key,
                            version: 2,
                            producerIncarnation,
                            ownerEpoch: expected.ownerEpoch,
                            reference: peer
                          })
                          result = { value: { state: 'stored' } }
                          return
                        }
                        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.peer-incarnation')
                      }
                      if (peer === null) {
                        result = { value: existing?.reference ?? null }
                        return
                      }
                      if (existing === null) {
                        store.add({
                          recordingId: key,
                          version: 2,
                          producerIncarnation,
                          ownerEpoch: expected.ownerEpoch,
                          reference: peer
                        })
                        result = { value: { state: 'stored' } }
                      } else
                        result = {
                          value: {
                            state:
                              webRecordingCanonical(existing.reference) === webRecordingCanonical(peer)
                                ? 'already-bound'
                                : 'conflict'
                          }
                        }
                    })
                })
            })
        }
      )
    } catch (error) {
      throw problem ?? error
    }
    if (problem !== undefined) throw problem
    if (result === undefined) throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-transaction')
    return result.value
  }
  return {
    bindOwned: async (owner, incarnation, input) => {
      const result = await owned(owner, incarnation, reference(input))
      if (result === null || !('state' in result))
        throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-transaction')
      return result
    },
    readOwned: async (owner, incarnation) => {
      const result = await owned(owner, incarnation, null)
      if (result !== null && 'state' in result)
        throw contractError('protocol.malformed', 'restoration', 'web-recording.peer-transaction')
      return result
    },
    read: id => transact(id, 'readonly', existing => existing),
    bind: (id, input) => {
      const peer = reference(input)
      return transact(id, 'readwrite', (existing, store, rowKey) => {
        if (existing !== null) {
          return {
            state:
              existing.version === peer.version &&
              existing.backendId === peer.backendId &&
              existing.scope === peer.scope &&
              existing.opaqueId === peer.opaqueId
                ? 'already-bound'
                : 'conflict'
          }
        }
        store.add({ recordingId: rowKey, version: 1, reference: peer })
        return { state: 'stored' }
      })
    }
  }
}

/** A durable fence remains held until the live owner has retired all receipts. */
export function webRecordingOwnerFenceKey(id: string): string {
  if (!isRecordingId(id)) throw contractError('argument.invalid', 'restoration', 'web-recording.owner-fence')
  return `:ubm-owner-fence-v1:${id}`
}

export async function webRecordingOwnerFence(
  database: WebOrderedJournalDatabase,
  owner: WebAuthorityOwner,
  producerIncarnation: string,
  action: 'hold' | 'release'
): Promise<void> {
  const key = webRecordingOwnerFenceKey(owner.id)
  let problem: unknown
  await runWebIndexedDbTransaction(
    database,
    ['recordings', 'authorityReservations', 'readerReceipts'],
    'readwrite',
    tx => {
      const guard = (fn: () => void): void => {
        try {
          fn()
        } catch (error) {
          problem = error
          tx.abort()
        }
      }
      const journal = tx.objectStore('recordings').get(owner.id)
      const reservation = tx.objectStore('authorityReservations').get(owner.id)
      const store = tx.objectStore('readerReceipts')
      const fence = store.get(key)
      fence.onsuccess = () =>
        guard(() => {
          const row = journal.result,
            authority = reservation.result,
            prior = fence.result
          const recovery =
            typeof authority === 'object' && authority !== null ? Reflect.get(authority, 'recoveryReceipt') : undefined
          const source = typeof recovery === 'object' && recovery !== null ? Reflect.get(recovery, 'source') : authority
          const receipt =
            typeof authority === 'object' && authority !== null ? Reflect.get(authority, 'receipt') : undefined
          const seal = typeof row === 'object' && row !== null ? Reflect.get(row, 'sealReceipt') : undefined
          if (
            typeof row !== 'object' ||
            row === null ||
            typeof authority !== 'object' ||
            authority === null ||
            Reflect.get(row, 'producerIncarnation') !== producerIncarnation ||
            Reflect.get(row, 'ownerEpoch') !== owner.ownerEpoch ||
            typeof source !== 'object' ||
            source === null ||
            Reflect.get(source, 'generation') !== owner.generation ||
            Reflect.get(source, 'ownerToken') !== owner.ownerToken ||
            Reflect.get(authority, 'ownerEpoch') !== owner.ownerEpoch ||
            (action === 'hold'
              ? Reflect.get(authority, 'phase') !== 'provisional' || recovery !== undefined
              : Reflect.get(authority, 'phase') !== 'released' ||
                typeof receipt !== 'object' ||
                receipt === null ||
                Reflect.get(receipt, 'transition') !== 'release' ||
                Reflect.get(receipt, 'phase') !== 'released' ||
                Reflect.get(row, 'ownerState') !== 'sealed' ||
                typeof seal !== 'object' ||
                seal === null ||
                Reflect.get(seal, 'generation') !== owner.generation ||
                Reflect.get(seal, 'ownerToken') !== owner.ownerToken ||
                Reflect.get(seal, 'ownerEpoch') !== owner.ownerEpoch)
          )
            throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.owner-fence')
          if (action === 'hold' && prior !== undefined)
            throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.owner-fence')
          if (
            action === 'release' &&
            prior !== undefined &&
            (typeof prior !== 'object' ||
              prior === null ||
              Reflect.get(prior, 'recordingId') !== key ||
              Reflect.get(prior, 'version') !== 1 ||
              Reflect.get(prior, 'producerIncarnation') !== producerIncarnation ||
              Reflect.get(prior, 'ownerEpoch') !== owner.ownerEpoch ||
              !['held', 'released'].includes(String(Reflect.get(prior, 'state'))))
          )
            throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.owner-fence')
          store.put({
            recordingId: key,
            version: 1,
            producerIncarnation,
            ownerEpoch: owner.ownerEpoch,
            state: action === 'hold' ? 'held' : 'released'
          })
        })
    }
  ).catch(error => {
    throw problem ?? error
  })
}

/** Remove only the exact untouched, revoked declaration and its own binding. */
export async function removeWebUnboundDeclaration(
  database: WebOrderedJournalDatabase,
  owner: WebAuthorityOwner,
  producerIncarnation: string,
  expectedPeer: WebRecordingPeerReference | null = null
): Promise<void> {
  let problem: unknown
  const cleanupKey = `:ubm-unbound-cleanup-v1:${owner.id}:${owner.ownerToken}:${owner.ownerEpoch}:${producerIncarnation}`
  const cleanupReceipt = {
    recordingId: cleanupKey,
    version: 1,
    generation: owner.generation,
    ownerToken: owner.ownerToken,
    ownerEpoch: owner.ownerEpoch,
    producerIncarnation,
    expectedPeer
  }
  await runWebIndexedDbTransaction(
    database,
    ['recordings', 'authorityReservations', 'readerReceipts'],
    'readwrite',
    tx => {
      const journal = tx.objectStore('recordings').get(owner.id)
      const authority = tx.objectStore('authorityReservations').get(owner.id)
      const peers = tx.objectStore('readerReceipts')
      const binding = peers.get(webRecordingPeerKey(owner.id))
      const fence = peers.get(webRecordingOwnerFenceKey(owner.id))
      const receipt = peers.get(owner.id)
      const cleanup = peers.get(cleanupKey)
      cleanup.onsuccess = () => {
        try {
          if (cleanup.result !== undefined) {
            if (webRecordingCanonical(cleanup.result) !== webRecordingCanonical(cleanupReceipt))
              throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.unbound-cleanup-receipt')
            return
          }
          const row = journal.result,
            reservation = authority.result,
            peer = parseWebRecordingPeerRow(binding.result, webRecordingPeerKey(owner.id)),
            priorFence = fence.result
          if (
            typeof row !== 'object' ||
            row === null ||
            typeof reservation !== 'object' ||
            reservation === null ||
            Reflect.get(row, 'id') !== owner.id ||
            Reflect.get(row, 'version') !== 1 ||
            Reflect.get(row, 'kind') !== 'ordered' ||
            Reflect.get(row, 'producerIncarnation') !== producerIncarnation ||
            Reflect.get(row, 'ownerEpoch') !== owner.ownerEpoch ||
            Reflect.get(row, 'ownerState') !== 'prepared' ||
            Reflect.get(row, 'nextOrdinal') !== 1 ||
            Reflect.get(row, 'recordCount') !== 0 ||
            Reflect.get(row, 'pendingCount') !== 0 ||
            Reflect.get(row, 'readerCursor') !== 0 ||
            Reflect.get(row, 'byteCount') !== 0 ||
            Reflect.get(row, 'pendingBytes') !== 0 ||
            Reflect.get(row, 'terminal') !== false ||
            Reflect.get(row, 'failure') !== null ||
            Reflect.get(row, 'openReceipt') !== undefined ||
            Reflect.get(row, 'terminalOutcome') !== undefined ||
            webRecordingCanonical(Reflect.get(row, 'readerIntegrity')) !==
              webRecordingCanonical({
                version: 3,
                receipt: null,
                accountedBytes: 0,
                pendingCount: 0,
                pendingBytes: 0,
                deliveryId: null,
                sourceOrdinal: null
              }) ||
            Reflect.get(row, 'lastDeliveryId') !== null ||
            Reflect.get(row, 'lastRetiredId') !== null ||
            Reflect.get(row, 'lastSourceOrdinal') !== null ||
            (peer !== null &&
              (expectedPeer === null ||
                peer.version !== 2 ||
                peer.producerIncarnation !== producerIncarnation ||
                peer.ownerEpoch !== owner.ownerEpoch ||
                webRecordingCanonical(peer.reference) !== webRecordingCanonical(expectedPeer))) ||
            (priorFence !== undefined &&
              (typeof priorFence !== 'object' ||
                priorFence === null ||
                Reflect.get(priorFence, 'recordingId') !== webRecordingOwnerFenceKey(owner.id) ||
                Reflect.get(priorFence, 'version') !== 1 ||
                Reflect.get(priorFence, 'producerIncarnation') !== producerIncarnation ||
                Reflect.get(priorFence, 'ownerEpoch') !== owner.ownerEpoch ||
                Reflect.get(priorFence, 'state') !== 'held')) ||
            receipt.result !== undefined ||
            Reflect.get(reservation, 'id') !== owner.id ||
            Reflect.get(reservation, 'generation') !== owner.generation ||
            Reflect.get(reservation, 'ownerToken') !== owner.ownerToken ||
            Reflect.get(reservation, 'ownerEpoch') !== owner.ownerEpoch ||
            Reflect.get(reservation, 'phase') !== 'released' ||
            typeof Reflect.get(reservation, 'receipt') !== 'object' ||
            Reflect.get(reservation, 'receipt') === null ||
            Reflect.get(Reflect.get(reservation, 'receipt'), 'transition') !== 'revoke' ||
            Reflect.get(Reflect.get(reservation, 'receipt'), 'phase') !== 'released' ||
            Reflect.get(reservation, 'declaration') !==
              webRecordingCanonical(
                [
                  'id',
                  'maxBytes',
                  'maxRecords',
                  'context',
                  'producerIncarnation',
                  'collectionId',
                  'sourceGeneration',
                  'memberEpochs'
                ].map(field => Reflect.get(row, field))
              )
          )
            throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.unbound-cleanup')
          peers.put(cleanupReceipt)
          tx.objectStore('recordings').delete(owner.id)
          tx.objectStore('authorityReservations').delete(owner.id)
          if (peer !== null) peers.delete(webRecordingPeerKey(owner.id))
          if (priorFence !== undefined) peers.delete(webRecordingOwnerFenceKey(owner.id))
        } catch (error) {
          problem = error
          tx.abort()
        }
      }
    }
  ).catch(error => {
    throw problem ?? error
  })
}
