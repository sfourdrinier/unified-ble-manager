// src/tck/approved-corrections.ts
//
// Separate store for TCK corrections (UBM 5.0 TCK card).
// A disagreement between the pinned reference and the actual contract is
// resolved to the contract and never silently preserved. This module is test
// infrastructure, never a second TCK.
//
// Scope note: no approved reference-bug correction is recorded here. Both
// entries below are DRAFT-side misunderstandings where the pinned reference
// was right and the first vector draft was wrong; each entry names the test
// that enforces the corrected property, and no unwired lookup helper
// survives (a store nobody reads cannot catch a regression).

export interface DraftTckCorrection {
  readonly correctionId: string
  readonly vectorId: string
  readonly scenarioId: string
  readonly observed: string
  readonly contractResolution: string
  readonly enforcedBy: string
  readonly recordedAt: string
}

function draftCorrection(
  correctionId: string,
  vectorId: string,
  scenarioId: string,
  observed: string,
  contractResolution: string,
  enforcedBy: string
): DraftTckCorrection {
  return Object.freeze({
    correctionId,
    vectorId,
    scenarioId,
    observed,
    contractResolution,
    enforcedBy,
    recordedAt: '2026-09-15'
  })
}

/**
 * Draft corrections discovered while adding UBM 5.0 race/bounds/cleanup
 * vectors. Each entry resolves a draft-vs-reference disagreement to the
 * actual contract and points at the test enforcing it.
 */
export const DRAFT_TCK_CORRECTIONS: readonly DraftTckCorrection[] = Object.freeze([
  draftCorrection(
    'draft-001-duplicate-completion-settles-once',
    'completion.duplicate-completion-settles-once',
    'lifecycle.destroy-idempotency-admission-and-exact-settlement',
    'Initial vector draft queued two completions for two independent reads, counted language-level promise settlements (tautological), and admitted any late-trace count up to 2.',
    'Contract requires exactly-once caller settlement with the abort-then-late ordering recorded exactly once as a late-success/late-failure trace. Vector issues one read against two queued completions and asserts settledOnce, exactly one late acknowledgement, and a healthy follow-up read.',
    'ubm5-race-bounds-cleanup duplicate-completion assertions + ubm5-draft-corrections duplicate-completion enforcement'
  ),
  draftCorrection(
    'draft-002-failed-cleanup-retains-and-reports',
    'cleanup.failed-cleanup-retains-and-reports',
    'subscription.pre-ready-overflow-controls-and-late-quarantine',
    'Initial draft treated an injected unsubscribe failure as a clean release; the first fix asserted the failure is reported but never proved the resource is retained.',
    'Contract forbids silently swallowing cleanup failures and silently dropping the resource. Vector asserts the failure is reported (release-failed or rejection), the subscription stays tracked per resource counters, and a follow-up remove() completes the release with zero residue.',
    'ubm5-race-bounds-cleanup failed-cleanup assertions + ubm5-draft-corrections failed-cleanup enforcement'
  )
])
