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

/**
 * One ref-vs-Rust gap, either closed by a transition-proving staged program
 * or staying open with a sharpened enforcedBy probe.
 *
 * - `transition-proven`: the scenario's BLE vocabulary now runs through the
 *   staged transition core from synthetic host events, and the parity suite
 *   asserts EQUAL observations ref-vs-Rust (same data bytes, error wires,
 *   receipts, sequencing). `transition` names the staged program and
 *   `observed` is the proof marker below (never a rejection wire).
 * - `stays-open-real-radio`: the scenario genuinely needs real radio (a
 *   host adapter, a live peer, or an OS event source the synthetic radio
 *   cannot project). The parity suite keeps pinning the loud
 *   `capability.unsupported|capability` rejection for its transition AND a
 *   staged probe proving the missing source (`staged` names the probe).
 *   Never silently closed, never faked.
 */
export interface RustParityGapCandidate {
  readonly correctionId: string
  readonly scenarioId: string
  readonly transition: string
  readonly observed: string
  readonly contractResolution: string
  readonly enforcedBy: string
  readonly recordedAt: string
  readonly status: 'transition-proven' | 'stays-open-real-radio'
  /** Staged program id (closed) or stay-open probe summary (open). */
  readonly staged: string
}

/** Proof marker for transition-proven candidates (not a rejection wire). */
export const RUST_PARITY_PROVEN_MARKER = 'transition-proven-by-staged-driver'

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

const PROVEN_RESOLUTION: string =
  'U7 gap CLOSED by the staged-transition slice: the scenario vocabulary runs through the ' +
  'session-owned staged transition core (a real Kernel+Central) from deterministic synthetic ' +
  'host events only (no BLE hardware). The parity suite asserts EQUAL observations ref-vs-Rust ' +
  'for the whole scripted program: same data bytes, same error wires, same settlement receipts, ' +
  'same effect sequencing, same bounded-batch accounting. Closed by proof, never by ' +
  're-description: the staged program id is recorded below and its pins fail the suite on ' +
  'any divergence.'

const ENFORCED_BY =
  '__tests__/tck/rust-parity/rust-parity.test.js U7 parity ' +
  '(staged transition proofs + sharpened stay-open probes + candidate coverage)'

function familyNoteFor(scenarioId: string): string {
  const note = FAMILY_NOTES[scenarioId]
  if (note === undefined) {
    throw new Error(`rust parity: missing family note for base scenario ${scenarioId}`)
  }
  return note
}

/**
 * Staged program closing one scenario (transition-proven), keyed by
 * scenario id. Absent means the scenario genuinely needs real radio and
 * stays open with a sharpened probe.
 */
const STAGED_PROGRAM_IDS: Readonly<Record<string, string>> = Object.freeze({
  'capability.truth-limits-evidence-and-binding': 'PROGRAM_CAPABILITY',
  'scan.owner-join-authority-and-signature': 'PROGRAM_SCAN_OWNER',
  'scan.fairness-abort-deadline-and-final-cleanup': 'PROGRAM_SCAN_FAIRNESS',
  'connection.lease-joins-borrowing-transfer-and-revocation': 'PROGRAM_CONNECTION_LEASE',
  'connection.two-client-arbitration': 'PROGRAM_CONNECTION_ARBITRATION',
  'gatt.discovery-complete-paths-and-services-changed': 'PROGRAM_GATT_DISCOVERY',
  'gatt.reads-descriptors-write-policy-and-dispatched-cancellation': 'PROGRAM_GATT_IO',
  'subscription.enable-ready-shared-cccd-and-fanout': 'PROGRAM_SUBSCRIPTION_FANOUT',
  'subscription.pre-ready-overflow-controls-and-late-quarantine': 'PROGRAM_SUBSCRIPTION_OVERFLOW',
  'lifecycle.destroy-idempotency-admission-and-exact-settlement': 'PROGRAM_LIFECYCLE_DESTROY',
  'scenario.scan-connect-discover-read-notify-destroy': 'PROGRAM_VERTICAL'
})

/** Sharpened stay-open reasons (what real radio still owns), keyed by id. */
const STAY_OPEN_REASONS: Readonly<Record<string, string>> = Object.freeze({
  'identity.provider-loadability-and-adapter-availability':
    'needs a real host adapter enumerating providers/adapters (staged probe: adapter.enumeration fails closed capability.unavailable)',
  'identity.adapter-selection-and-unique-instance':
    'needs a real host adapter with selectable instances (staged probe: adapter.enumeration fails closed capability.unavailable)',
  'identity.valid-all-axis-negotiation':
    'needs a live peer handshake across version axes (staged probe: no identity-scoped capability is projected)',
  'identity.version-skew-and-malformed-offers':
    'needs a live peer offering skewed/malformed versions (staged probe: no identity-scoped capability is projected)',
  'adapter.atomic-snapshot-and-watch':
    'needs a live OS adapter emitting state transitions (staged probe: adapter.watch fails closed capability.unavailable)',
  'diagnostics.trace-redaction-and-resource-counters':
    'needs real operation traffic through a redacting trace sink (staged probe: only bounded-batch accounting is exposed)'
})

function gapCandidate(scenarioId: string): RustParityGapCandidate {
  const definitions = rustParityBaseScenarios()
  const definition = definitions.find(candidate => candidate.id === scenarioId)
  if (definition === undefined) {
    throw new Error(`rust parity: base scenario is not registered: ${scenarioId}`)
  }
  const slug = scenarioId.replace(/[^a-z0-9]+/gu, '-')
  const programId = STAGED_PROGRAM_IDS[scenarioId]
  if (programId !== undefined) {
    return Object.freeze({
      correctionId: `u7-gap-${slug}`,
      scenarioId,
      transition: programId,
      observed: RUST_PARITY_PROVEN_MARKER,
      contractResolution: `${PROVEN_RESOLUTION} Family: ${familyNoteFor(scenarioId)}.`,
      enforcedBy: ENFORCED_BY,
      recordedAt: '2026-09-16',
      status: 'transition-proven' as const,
      staged: programId
    })
  }
  const reason = STAY_OPEN_REASONS[scenarioId]
  if (reason === undefined) {
    throw new Error(`rust parity: scenario is neither staged-proven nor stay-open: ${scenarioId}`)
  }
  return Object.freeze({
    correctionId: `u7-gap-${slug}`,
    scenarioId,
    transition: gapTransitionFor(definition),
    observed: RUST_PARITY_UNWIRED_WIRE,
    contractResolution: `${SHARED_RESOLUTION} Family: ${familyNoteFor(scenarioId)}. ` + `Stay-open reason: ${reason}.`,
    enforcedBy: ENFORCED_BY,
    recordedAt: '2026-09-16',
    status: 'stays-open-real-radio',
    staged: reason
  })
}

/** Gap candidates for every frozen base scenario, in corpus order. */
export const RUST_PARITY_GAP_CANDIDATES: readonly RustParityGapCandidate[] = Object.freeze(
  rustParityBaseScenarios().map(definition => gapCandidate(definition.id))
)
