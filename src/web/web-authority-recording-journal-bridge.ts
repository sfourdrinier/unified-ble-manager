import { webRecordingRandomBytes } from './web-recording-errors'
import { createWebRecordingMarkerLedger } from './web-recording-marker-ledger'
import { boundedWebRecordingOperation } from './web-indexeddb-transaction'
import { webRecordingContractError as contractError } from './web-recording-errors'
import { createWebRecordingAuthority } from './web-recording-authority'
import type { WebRecordingAuthority, WebRecordingAuthorityOptions } from './web-recording-authority'
import { createWebOrderedRecordingJournal, webRecordingCanonical } from './web-ordered-recording-journal'
import type {
  WebOrderedJournalDatabase,
  WebOrderedRecordingDeclaration,
  WebOrderedRecordingProducer
} from './web-ordered-recording-journal'
import type { WebRecordingTerminalObservation } from './web-ordered-recording-journal'
import type { OrderedRecordingHostStopResult } from '../core/continuation-recording'
import { createWebAuthorityToken } from './web-authority-reservation'
import { createWebRecordingJournalBridge } from './web-recording-journal-bridge'
import { snapshotPeerReference } from '../backend-contract/peer-reference'
import { parseOrderedRecordingContext } from '../core/ordered-recording'
import { removeWebUnboundDeclaration, webRecordingOwnerFence } from './web-recording-peer-identity'
import type { WebRecordingPeerIdentityStore, WebRecordingPeerReference } from './web-recording-peer-identity'

/** Retain one operation identity across an unobserved completion. Reconcile
 * only after open was actually dispatched; a row status is not its receipt. */
export function createWebRecordingOpenOperation(journal: WebOrderedRecordingProducer, operationId: string) {
  let started = false
  return {
    open: (): Promise<void> => {
      if (started) return Promise.reject(contractError('lifecycle.invalid-state', 'restoration', 'web-recording.open'))
      started = true
      return journal.open(operationId)
    },
    reconcileOpen: (): Promise<'committed' | 'aborted' | 'unknown'> =>
      started ? journal.reconcileOpen(operationId) : Promise.resolve('unknown')
  }
}

/** Internal live-owner adapter. The caller must synchronously fence its source
 * from options.onLoss and retain the returned supervisor through cleanup. */
export function createWebAuthorityRecordingJournalBridge(
  options: WebRecordingAuthorityOptions,
  input: Omit<WebOrderedRecordingDeclaration, 'memberEpochs' | 'ownerEpoch' | 'producerIncarnation'>,
  crypto: { getRandomValues(bytes: Uint8Array): Uint8Array },
  peerBinding?: { readonly store: WebRecordingPeerIdentityStore; readonly reference: WebRecordingPeerReference },
  onDeclared?: (identity: {
    readonly recordingId: string
    readonly producerIncarnation: string
    readonly ownerEpoch: string
  }) => void,
  onProvisional?: (identity: {
    readonly recordingId: string
    readonly producerIncarnation: string
    readonly ownerEpoch: string
  }) => void
) {
  const declaration = { ...input, context: parseOrderedRecordingContext(input.context) }
  let authority: WebRecordingAuthority | null = null
  let authorityLost = false
  let declarationDispatchPending = false
  let declarationAttempt = 0
  const authorityDatabase: WebOrderedJournalDatabase = {
    ready: async () => {
      const isDeclaration = declarationDispatchPending
      const attempt = declarationAttempt
      if (isDeclaration) declarationDispatchPending = false
      await options.database.ready?.()
      if (isDeclaration && (authorityLost || attempt !== declarationAttempt))
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
    },
    transaction: options.database.transaction.bind(options.database)
  }
  const heldOwnerDatabase: WebOrderedJournalDatabase = {
    ready: async () => {
      if (authority?.current() !== true)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      await options.database.ready?.()
      if (authority?.current() !== true)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
    },
    transaction: (stores, mode, transactionOptions) => {
      if (authority?.current() !== true)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      return options.database.transaction(stores, mode, transactionOptions)
    }
  }
  let disposition: 'none' | 'held' | 'release-unproven' = 'none'
  let entropy: Uint8Array | null = null
  let openOperation: ReturnType<typeof createWebRecordingOpenOperation> | undefined
  let lossHandler: (() => void) | undefined
  let lastObservation: OrderedRecordingHostStopResult | null = null
  let observationOperation: string | null = null
  let unboundOwner: ReturnType<WebRecordingAuthority['reservationOwner']> | null = null
  let unboundIncarnation: string | null = null
  let unboundRemoved = false
  let provisionalPeer: WebRecordingPeerReference | null = null
  let uncertainBinding: {
    readonly store: WebRecordingPeerIdentityStore
    readonly owner: ReturnType<WebRecordingAuthority['reservationOwner']>
    readonly incarnation: string
  } | null = null
  let fenceOwner: ReturnType<WebRecordingAuthority['reservationOwner']> | null = null
  let fenceIncarnation: string | null = null
  const reference =
    peerBinding === undefined ? null : snapshotPeerReference(peerBinding.reference, 'web-recording.peer')
  if (reference !== null && reference.scope !== 'origin')
    throw contractError('peer.reference-invalid', 'restoration', 'web-recording.peer')
  const bridge = createWebRecordingJournalBridge(
    options.database,
    async memberEpochs => {
      if (authority === null || entropy === null)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      const random = entropy
      const incarnation = authority.createProducerIncarnation({
        getRandomValues: bytes => {
          bytes.set(random)
          return bytes
        }
      })
      const full = { ...declaration, producerIncarnation: incarnation, memberEpochs }
      declarationDispatchPending = true
      let result: Awaited<ReturnType<WebRecordingAuthority['declare']>>
      try {
        result = await authority.declare(full)
      } finally {
        declarationDispatchPending = false
        declarationAttempt++
      }
      if (result.state !== 'prepared')
        throw contractError('lifecycle.invariant-violation', 'restoration', 'web-recording.declaration')
      const identity = { recordingId: declaration.id, producerIncarnation: incarnation, ownerEpoch: result.ownerEpoch }
      onProvisional?.(identity)
      if (peerBinding !== undefined && reference !== null && reference.scope === 'origin') {
        const { store } = peerBinding
        if (store.bindOwned === undefined || store.readOwned === undefined)
          throw contractError('capability.unavailable', 'restoration', 'web-recording.peer-authority')
        const owner = authority.reservationOwner()
        const peer = { ...reference, scope: 'origin' as const }
        if (onProvisional !== undefined) {
          unboundOwner = owner
          unboundIncarnation = incarnation
          provisionalPeer = peer
        }
        let state: 'stored' | 'already-bound' | 'conflict'
        let timer: ReturnType<typeof setTimeout> | undefined
        try {
          state = (
            await Promise.race([
              store.bindOwned(owner, incarnation, peer),
              new Promise<never>((_resolve, reject) => {
                timer = setTimeout(() => reject(new Error('Web recording peer binding outcome remains unknown')), 5000)
              })
            ])
          ).state
        } catch (error) {
          let stored: WebRecordingPeerReference | null
          try {
            stored = await store.readOwned(owner, incarnation)
            if (stored !== null) snapshotPeerReference(stored, 'web-recording.peer-reconcile')
          } catch (readError) {
            if (onProvisional !== undefined) uncertainBinding = { store, owner, incarnation }
            throw new AggregateError([error, readError], 'Web recording peer binding remains uncertain')
          }
          if (stored === null) {
            unboundOwner = owner
            unboundIncarnation = incarnation
          }
          if (
            stored === null ||
            stored.version !== peer.version ||
            stored.backendId !== peer.backendId ||
            stored.scope !== peer.scope ||
            stored.opaqueId !== peer.opaqueId
          )
            throw error
          state = 'already-bound'
        } finally {
          clearTimeout(timer)
        }
        if (state !== 'stored' && state !== 'already-bound')
          throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.peer-conflict')
        if (!authority.current())
          throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      }
      if (peerBinding !== undefined) {
        fenceOwner = authority.reservationOwner()
        fenceIncarnation = incarnation
        await webRecordingOwnerFence(heldOwnerDatabase, fenceOwner, incarnation, 'hold')
      }
      onDeclared?.(identity)
      unboundOwner = null
      unboundIncarnation = null
      provisionalPeer = null
      return { ...declaration, producerIncarnation: incarnation, ownerEpoch: result.ownerEpoch }
    },
    async signal => {
      // Obtain entropy before acquiring the lock, so a failing entropy source
      // cannot strand ownership before the supervisor receives the lease.
      entropy = webRecordingRandomBytes(crypto, new Uint8Array(32))
      if (entropy.length !== 32) throw contractError('capability.unavailable', 'restoration', 'web-recording.entropy')
      const result = await createWebRecordingAuthority(
        {
          ...options,
          database: authorityDatabase,
          signal,
          onLoss: error => {
            authorityLost = true
            lossHandler?.()
            options.onLoss?.(error)
          }
        },
        declaration.id,
        state => {
          disposition = state
        }
      )
      if (result.state === 'busy') {
        throw contractError('capability.unavailable', 'restoration', 'web-recording.authority-busy')
      }
      authority = result.authority
      return authority
    },
    (database, full) => {
      if (authority === null)
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.reservation-required')
      const owner = authority.reservationOwner()
      const journal = createWebOrderedRecordingJournal(database, full, owner)
      openOperation = createWebRecordingOpenOperation(journal, createWebAuthorityToken(crypto))
      return {
        ...journal,
        open: async () => {
          await openOperation?.open()
          authority?.noteOpenCommitted()
        }
      }
    },
    () => disposition === 'none'
  )
  const removeUnbound = boundedWebRecordingOperation(async () => {
    if (unboundOwner === null || unboundIncarnation === null)
      throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.unbound-cleanup')
    await removeWebUnboundDeclaration(options.database, unboundOwner, unboundIncarnation, provisionalPeer)
  })
  let pendingObservation: Omit<WebRecordingTerminalObservation, 'deliveryId' | 'operationId'> | null = null
  let observationDispatch: Promise<boolean> | null = null
  const retainObservation = boundedWebRecordingOperation(async () => {
    if (pendingObservation === null || observationOperation === null)
      throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.terminal-observation')
    const observation = pendingObservation
    let markDispatch!: (dispatched: boolean) => void
    observationDispatch = new Promise(resolve => {
      markDispatch = resolve
    })
    const attempt = bridge.observeTerminal({ ...observation, operationId: observationOperation }, () => {
      markDispatch(true)
    })
    attempt.then(
      () => markDispatch(false),
      () => markDispatch(false)
    )
    await attempt
    lastObservation = observation.result
  }, 1)
  const reconcileObservation = boundedWebRecordingOperation(async () => {
    if (pendingObservation === null || observationOperation === null)
      throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.terminal-observation')
    const observation = pendingObservation
    const forgetAttempt = retainObservation.pendingSnapshot()
    const dispatch = observationDispatch
    // Only a read created after this exact write transaction can fence it.
    // A failed or not-yet-dispatched attempt supplies no absence evidence.
    if (dispatch === null || !(await dispatch)) return 'unknown'
    const outcome = await bridge.reconcileTerminal(observationOperation, observation.result)
    if (outcome === 'committed') lastObservation = observation.result
    if (outcome === 'absent') forgetAttempt()
    return outcome
  }, 1)
  let retirementResult: OrderedRecordingHostStopResult | null = null
  let authorityReleased = false
  const checkRetirement = boundedWebRecordingOperation(async () => {
    if (
      retirementResult === null ||
      observationOperation === null ||
      !(await bridge.terminalRetained(observationOperation, retirementResult))
    )
      return false
    if (fenceOwner !== null && fenceIncarnation !== null)
      await webRecordingOwnerFence(options.database, fenceOwner, fenceIncarnation, 'release')
    return true
  })
  return {
    ...bridge,
    markerLedger: () => {
      if (authority === null || fenceIncarnation === null || !authority.current())
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.marker-owner')
      return createWebRecordingMarkerLedger(heldOwnerDatabase, authority.reservationOwner(), fenceIncarnation)
    },
    releaseAuthority: async (): Promise<void> => {
      if (uncertainBinding !== null) {
        const pending = uncertainBinding
        if (pending.store.readOwned === undefined)
          throw contractError('capability.unavailable', 'restoration', 'web-recording.peer-authority')
        const stored = await pending.store.readOwned(pending.owner, pending.incarnation)
        if (
          stored !== null &&
          (provisionalPeer === null ||
            stored.version !== provisionalPeer.version ||
            stored.backendId !== provisionalPeer.backendId ||
            stored.scope !== provisionalPeer.scope ||
            stored.opaqueId !== provisionalPeer.opaqueId)
        )
          throw contractError('capability.unavailable', 'restoration', 'web-recording.peer-binding-uncertain')
        unboundOwner = pending.owner
        unboundIncarnation = pending.incarnation
        uncertainBinding = null
      }
      if (!authorityReleased) {
        await bridge.releaseAuthority()
        authorityReleased = true
      }
      if (
        onDeclared === undefined &&
        lastObservation !== null &&
        observationOperation !== null &&
        fenceOwner !== null &&
        fenceIncarnation !== null
      ) {
        retirementResult ??= lastObservation
        if (!(await checkRetirement()))
          throw contractError('capability.unavailable', 'restoration', 'web-recording.terminal-unproven')
      }
      if (unboundOwner !== null && unboundIncarnation !== null) {
        await removeUnbound()
        unboundRemoved = true
        unboundOwner = null
        unboundIncarnation = null
      }
    },
    unboundDeclarationRemoved: (): boolean => unboundRemoved,
    observeTerminal: async (
      observation: Omit<WebRecordingTerminalObservation, 'deliveryId' | 'operationId'>
    ): Promise<void> => {
      observationOperation ??= createWebAuthorityToken(crypto)
      if (
        pendingObservation !== null &&
        webRecordingCanonical(pendingObservation) !== webRecordingCanonical(observation)
      )
        throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.terminal-observation-conflict')
      pendingObservation ??= observation
      await retainObservation()
    },
    reconcileObservation,
    terminalRetained: async (result: OrderedRecordingHostStopResult): Promise<boolean> => {
      if (retirementResult !== null && webRecordingCanonical(retirementResult) !== webRecordingCanonical(result))
        return false
      retirementResult ??= result
      return checkRetirement()
    },
    onAuthorityLoss: (handler: () => void): void => {
      lossHandler = handler
    },
    beginCleanup: async (): Promise<void> => {
      if (authority === null) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      await authority.beginCleanup()
    },
    canRetrySeal: (): boolean => true,
    seal: async (): Promise<void> => {
      if (authority === null) throw contractError('lifecycle.invalid-state', 'restoration', 'web-recording.authority')
      await authority.beginCleanup()
      await bridge.seal()
    },
    reconcileOpen: async (): Promise<'committed' | 'aborted' | 'unknown'> => {
      const result = openOperation === undefined ? 'aborted' : await openOperation.reconcileOpen()
      if (result === 'committed') authority?.noteOpenCommitted()
      return result
    }
  }
}
