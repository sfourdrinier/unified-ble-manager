// src/tck/approved-corrections.ts
//
// Separate store for approved old-bug corrections (UBM 5.0 TCK card).
// A disagreement between the pinned reference and the actual contract is
// resolved to the contract, the reference bug is noted here, and never
// silently preserved. This module is test infrastructure, never a second TCK.

export interface ApprovedTckCorrection {
  readonly vectorId: string
  readonly scenarioId: string
  readonly observed: string
  readonly contractResolution: string
  readonly referenceBugNoted: boolean
  readonly recordedAt: string
}

function correction(
  vectorId: string,
  scenarioId: string,
  observed: string,
  contractResolution: string,
  referenceBugNoted: boolean
): ApprovedTckCorrection {
  return Object.freeze({
    vectorId,
    scenarioId,
    observed,
    contractResolution,
    referenceBugNoted,
    recordedAt: '2026-09-15'
  })
}

/**
 * Approved corrections discovered while adding UBM 5.0 race/bounds/cleanup
 * vectors. Each entry resolves a reference-vs-contract disagreement to the
 * actual contract.
 */
export const APPROVED_TCK_CORRECTIONS: readonly ApprovedTckCorrection[] = Object.freeze([
  correction(
    'completion.duplicate-completion-settles-once',
    'lifecycle.destroy-idempotency-admission-and-exact-settlement',
    'Initial vector draft expected a late duplicate completion to reject the already-settled read; the pinned reference settles the read once and records the duplicate as a late-acknowledgement trace.',
    'Contract requires exactly-once settlement with late duplicates observed as late-success/late-failure traces, not as a second settlement. Vector asserts settledOnce and noDoubleSettlement.',
    false
  ),
  correction(
    'cleanup.failed-cleanup-retains-and-reports',
    'subscription.pre-ready-overflow-controls-and-late-quarantine',
    'Initial draft treated an injected unsubscribe failure as a clean release; the pinned reference reports release-failed with a subscription failure instead of released.',
    'Contract forbids silently swallowing cleanup failures. Vector asserts the failure is retained and reported (release-failed or rejection), never a clean released.',
    false
  )
])

export function findApprovedCorrection(vectorId: string): ApprovedTckCorrection | null {
  for (const entry of APPROVED_TCK_CORRECTIONS) {
    if (entry.vectorId === vectorId) {
      return entry
    }
  }
  return null
}
