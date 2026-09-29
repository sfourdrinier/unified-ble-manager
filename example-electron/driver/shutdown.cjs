'use strict'

/** Attempt each process owner without converting rejection or refusal into
 * success. Callers keep these same owners alive for retry on a failed receipt. */
async function shutdownOwners(owners) {
  const outcomes = []
  for (const owner of owners) {
    try {
      const receipt = await owner.destroy()
      if (
        receipt === null ||
        typeof receipt !== 'object' ||
        (receipt.state !== 'released' && receipt.state !== 'release-failed') ||
        !Array.isArray(receipt.failures) ||
        (receipt.state === 'released' && receipt.failures.length !== 0)
      ) {
        throw new Error(`${owner.name} returned no valid cleanup receipt`)
      }
      outcomes.push({ name: owner.name, receipt })
    } catch (error) {
      outcomes.push({ name: owner.name, error })
    }
  }
  return {
    state: outcomes.every(outcome => outcome.receipt?.state === 'released') ? 'released' : 'release-failed',
    outcomes
  }
}

/** Radio release is not a data handoff. Keep the independent process control
 * bridge/window alive on failure so an explicit renderer claim can resolve it. */
async function shutdownProcessSession(session) {
  const cleanup = await session.destroy()
  const handoff = await shutdownOwners([
    {
      name: 'continuation.handoff',
      destroy: async () => {
        const host = await session.allocatedHost()
        if (host !== null && (await host.continuation.status()) !== null) {
          throw new Error(
            'continuation data remains owned; perform an explicit process-continuation claim before quitting'
          )
        }
        return { state: 'released', failures: [] }
      }
    }
  ])
  return {
    state: cleanup.state === 'released' && handoff.state === 'released' ? 'released' : 'release-failed',
    outcomes: [...cleanup.outcomes, ...handoff.outcomes]
  }
}

module.exports = { shutdownOwners, shutdownProcessSession }
