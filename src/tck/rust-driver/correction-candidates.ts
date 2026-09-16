// src/tck/rust-driver/correction-candidates.ts
//
// U7 parity slice: approved-correction candidates for the ref-vs-Rust BLE
// surface (one per frozen base scenario, derived from the corpus so the list
// can never drift from it).
//
// Each entry records a ref-vs-Rust DISAGREEMENT resolved to the actual
// protocol/contract: the reference backend performs the scenario's BLE
// transitions while the transition-driving core loudly reports the frozen
// `capability.unsupported|capability` identity for the same vocabulary
// (this boundary has no radio/host executor yet). That rejection is the
// correct contract behavior for an unsupported capability — never a silent
// equality, never a weakened expectation. The parity suite pins each exact
// wire identity and fails if a candidate is missing, stale, or unwired
// (mirroring the `DRAFT_TCK_CORRECTIONS` enforced-by pattern: a store
// nobody reads cannot catch a regression).
//
// Candidates resolve when the transition is wired through the `CoreBackend`
// seam against a radio/host executor (gate ledger U7/U10); until then the
// suite pins these exact rejections.

import { gapTransitionFor, RUST_PARITY_UNWIRED_WIRE, rustParityBaseScenarios } from './corpus'

/** One ref-vs-Rust disagreement resolved to the actual contract. */
export interface RustParityGapCandidate {
  readonly correctionId: string
  readonly scenarioId: string
  readonly transition: string
  readonly observed: string
  readonly contractResolution: string
  readonly enforcedBy: string
  readonly recordedAt: string
}

/** Scenario-family note: which BLE surface the gap covers. */
const FAMILY_NOTES: Readonly<Record<string, string>> = Object.freeze({
  'identity.provider-loadability-and-adapter-availability':
    'provider/adapter identity surface (no adapter enumeration on this boundary)',
  'identity.adapter-selection-and-unique-instance':
    'adapter selection surface (no adapter enumeration on this boundary)',
  'identity.valid-all-axis-negotiation': 'version-axis negotiation surface (no peer handshake on this boundary)',
  'identity.version-skew-and-malformed-offers': 'version-skew surface (no peer handshake on this boundary)',
  'capability.truth-limits-evidence-and-binding':
    'capability runtime-truth surface (no host capabilities on this boundary)',
  'adapter.atomic-snapshot-and-watch': 'adapter watch surface (no adapter state source on this boundary)',
  'scan.owner-join-authority-and-signature': 'scan physical-authority surface (no radio on this boundary)',
  'scan.fairness-abort-deadline-and-final-cleanup': 'scan lifecycle surface (no radio on this boundary)',
  'connection.lease-joins-borrowing-transfer-and-revocation':
    'connection lease surface (no link layer on this boundary)',
  'connection.two-client-arbitration': 'connection arbitration surface (no link layer on this boundary)',
  'gatt.discovery-complete-paths-and-services-changed': 'GATT discovery surface (no ATT transport on this boundary)',
  'gatt.reads-descriptors-write-policy-and-dispatched-cancellation':
    'GATT operation surface (no ATT transport on this boundary)',
  'subscription.enable-ready-shared-cccd-and-fanout': 'subscription fanout surface (no ATT transport on this boundary)',
  'subscription.pre-ready-overflow-controls-and-late-quarantine':
    'subscription overflow surface (no ATT transport on this boundary)',
  'lifecycle.destroy-idempotency-admission-and-exact-settlement':
    'operation admission/settlement surface (no admitted operations in this slice)',
  'diagnostics.trace-redaction-and-resource-counters': 'diagnostics trace surface (no operation traffic in this slice)',
  'scenario.scan-connect-discover-read-notify-destroy': 'vertical slice surface (no radio/ATT stack on this boundary)'
})

const SHARED_RESOLUTION: string =
  'U7 gap, not a contract violation: the transition-driving core holds and drives a real ' +
  'Kernel+Central for status/sweep/destroy, but BLE transitions need a radio/host executor ' +
  '(gate ledger U7/U10). The binding reports the actual frozen capability.unsupported|capability ' +
  'identity instead of faking support. Resolves when the transition is wired through the ' +
  'CoreBackend seam; until then the parity suite pins this exact rejection.'

const ENFORCED_BY = '__tests__/tck/rust-parity/rust-parity.test.js U7 parity (gap probes + candidate coverage)'

function familyNoteFor(scenarioId: string): string {
  const note = FAMILY_NOTES[scenarioId]
  if (note === undefined) {
    throw new Error(`rust parity: missing family note for base scenario ${scenarioId}`)
  }
  return note
}

function gapCandidate(scenarioId: string): RustParityGapCandidate {
  const definitions = rustParityBaseScenarios()
  const definition = definitions.find(candidate => candidate.id === scenarioId)
  if (definition === undefined) {
    throw new Error(`rust parity: base scenario is not registered: ${scenarioId}`)
  }
  const slug = scenarioId.replace(/[^a-z0-9]+/gu, '-')
  return Object.freeze({
    correctionId: `u7-gap-${slug}`,
    scenarioId,
    transition: gapTransitionFor(definition),
    observed: RUST_PARITY_UNWIRED_WIRE,
    contractResolution: `${SHARED_RESOLUTION} Family: ${familyNoteFor(scenarioId)}.`,
    enforcedBy: ENFORCED_BY,
    recordedAt: '2026-09-16'
  })
}

/** Gap candidates for every frozen base scenario, in corpus order. */
export const RUST_PARITY_GAP_CANDIDATES: readonly RustParityGapCandidate[] = Object.freeze(
  rustParityBaseScenarios().map(definition => gapCandidate(definition.id))
)
