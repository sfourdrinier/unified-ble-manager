'use strict'

const { shutdownOwners } = require('./shutdown.cjs')

/** Trusted application lifetime adapter. Factories retain failed partial
 * allocations through retryCleanup; ordinary rejection owns no new resource.
 * createBinding must retain/compensate partial construction before rejecting.
 * No operation here claims volatile data or acknowledges/deletes journals. */
function createProcessSession(factories) {
  let closing = false
  let hostPending = null
  let bindingPending = null
  let destruction = null
  const accepted = new Set()
  const owners = new Map()
  const admit = () => {
    if (closing) throw new Error('Electron process session is closed')
  }
  const released = receipt =>
    receipt?.state === 'released' && Array.isArray(receipt.failures) && receipt.failures.length === 0
  const track = promise => {
    accepted.add(promise)
    void promise.then(
      () => accepted.delete(promise),
      () => accepted.delete(promise)
    )
    return promise
  }
  const retain = (name, rank, destroy) => owners.set(name, { name, rank, destroy })
  const retainRetry = (error, name, rank, onReleased = () => {}) => {
    if (error === null || typeof error !== 'object' || typeof error.retryCleanup !== 'function') return false
    retain(name, rank, async () => {
      const receipt = await error.retryCleanup()
      if (released(receipt)) onReleased()
      return receipt
    })
    return true
  }
  const ensureHost = () => {
    if (hostPending === null) {
      const opening = Promise.resolve()
        .then(() => factories.createProcessHost())
        .then(
          host => {
            retain('process-host.destroy', 0, () => host.destroy())
            return host
          },
          error => {
            if (
              !retainRetry(error, 'process-host.initialization', 0, () => {
                hostPending = null
              }) &&
              hostPending === opening
            )
              hostPending = null
            throw error
          }
        )
      hostPending = track(opening)
    }
    return hostPending
  }
  return Object.freeze({
    processHost: async () => {
      admit()
      const host = await ensureHost()
      admit()
      return host
    },
    allocatedHost: async () => (hostPending === null ? null : hostPending),
    binding: async () => {
      admit()
      if (bindingPending === null) {
        const opening = (async () => {
          const host = await ensureHost()
          admit()
          let manager
          try {
            manager = await host.createInternalManager()
          } catch (error) {
            retainRetry(error, 'manager.initialization', 1)
            throw error
          }
          retain('manager.destroy', 1, () => manager.destroy())
          admit()
          let binding
          try {
            binding = await factories.createBinding(manager)
          } catch (error) {
            retainRetry(error, 'binding.initialization', 2)
            throw error
          }
          retain('binding.destroy', 2, () => binding.destroy())
          return binding
        })().catch(error => {
          if (hostPending === null && bindingPending === opening) bindingPending = null
          throw error
        })
        bindingPending = track(opening)
      }
      const binding = await bindingPending
      admit()
      return binding
    },
    recordings: () => factories.openRecordings(),
    destroy: () => {
      closing = true
      if (destruction === null) {
        destruction = (async () => {
          const attempted = new Set()
          const releaseKnown = () =>
            Promise.all(
              [...owners.values()]
                .filter(owner => !attempted.has(owner))
                .sort((a, b) => b.rank - a.rank)
                .map(async owner => {
                  attempted.add(owner)
                  const result = await shutdownOwners([owner])
                  const outcome = result.outcomes[0]
                  if (released(outcome.receipt) && owners.get(owner.name) === owner) owners.delete(owner.name)
                  return outcome
                })
            )
          // Reach authoritative native shutdown now, even if a local borrower or
          // binding factory is held. Final completion still waits for all accepted
          // initialization and cleans late resources by their exact owner identity.
          const initial = releaseKnown()
          await Promise.allSettled([...accepted])
          const late = releaseKnown()
          const outcomes = [...(await initial), ...(await late)]
          return {
            state: outcomes.every(outcome => released(outcome.receipt)) ? 'released' : 'release-failed',
            outcomes
          }
        })().then(
          result => {
            if (result.state !== 'released') destruction = null
            return result
          },
          error => {
            destruction = null
            throw error
          }
        )
      }
      return destruction
    }
  })
}

module.exports = { createProcessSession }
