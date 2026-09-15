// contracts/src/semantic-map.ts — C-UBM DRAFT (pending U1 acceptance).
//
// 4.x semantic map plus the approved-corrections log. Every
// docs/UNIFIED_SEMANTICS.md section maps to a C-UBM module; disagreements
// are recorded as approved corrections against the protocol/contract, never
// as silent equality.

export type MapDisposition = 'retained' | 'corrected' | 'extended' | 'noted';

export interface SemanticMapping {
  readonly section: number;
  readonly subject: string;
  readonly disposition: MapDisposition;
  readonly cubmModule: string;
  readonly correctionId: string | null;
  readonly note: string;
}

export const SEMANTIC_MAP: readonly SemanticMapping[] = [
  { section: 1, subject: 'authority, inert entry, structural records', disposition: 'retained', cubmModule: 'version, identities', correctionId: null, note: 'package version is not a handshake axis' },
  { section: 2, subject: 'vocabulary, identity, version negotiation', disposition: 'corrected', cubmModule: 'version, identities', correctionId: 'AC-01', note: 'structural records; revision equality explicit' },
  { section: 3, subject: 'ownership and multi-client arbitration', disposition: 'retained', cubmModule: 'hosts, transitions', correctionId: null, note: 'OWN-01 rulings frozen' },
  { section: 4, subject: 'lifecycle state machines', disposition: 'retained', cubmModule: 'transitions', correctionId: null, note: 'all nine machines frozen' },
  { section: 5, subject: 'adapter state, permission, reset', disposition: 'retained', cubmModule: 'hosts', correctionId: null, note: 'single isAuthorizationBlocking predicate' },
  { section: 6, subject: 'scan sessions', disposition: 'retained', cubmModule: 'central, streams', correctionId: null, note: 'unsupported filters reject' },
  { section: 7, subject: 'chooser sessions', disposition: 'retained', cubmModule: 'transitions', correctionId: null, note: 'chooser distinct from scan' },
  { section: 8, subject: 'connections, adoption, disconnect', disposition: 'retained', cubmModule: 'hosts, identities', correctionId: null, note: 'generations invalidate before terminal' },
  { section: 9, subject: 'discovery, database epochs, paths', disposition: 'retained', cubmModule: 'identities, central', correctionId: null, note: 'occurrence identity; UUID canonicalization' },
  { section: 10, subject: 'GATT I/O, descriptors, subscriptions', disposition: 'retained', cubmModule: 'central, peripheral', correctionId: null, note: 'mandatory write mode; managed CCCD' },
  { section: 11, subject: 'bounded streams and overflow', disposition: 'retained', cubmModule: 'streams, bounds', correctionId: null, note: 'STR-01 defaults frozen' },
  { section: 12, subject: 'bytes, ownership, boundary limits', disposition: 'corrected', cubmModule: 'bounds', correctionId: 'AC-03', note: '524288 effective ceiling; decimal-string u64' },
  { section: 13, subject: 'operations, cancellation, deadlines', disposition: 'retained', cubmModule: 'effects, bounds, outcomes', correctionId: null, note: 'OPS-01/OPS-02 arbitration' },
  { section: 14, subject: 'race arbitration and happens-before', disposition: 'retained', cubmModule: 'effects', correctionId: null, note: 'first valid contender wins' },
  { section: 15, subject: 'errors and platform detail', disposition: 'retained', cubmModule: 'outcomes', correctionId: null, note: 'full code catalog; safe detail only' },
  { section: 16, subject: 'capabilities, limitations, evidence', disposition: 'retained', cubmModule: 'capabilities', correctionId: null, note: 'four-state vocabulary; runtime authority' },
  { section: 17, subject: 'permission, background, bond, MTU, RSSI', disposition: 'retained', cubmModule: 'central, capabilities', correctionId: null, note: 'request vs observation distinct' },
  { section: 18, subject: 'restoration and exact replay', disposition: 'retained', cubmModule: 'streams, transitions', correctionId: null, note: 'error-policy replay stream' },
  { section: 19, subject: 'reset, restart, replacement', disposition: 'retained', cubmModule: 'transitions, version', correctionId: null, note: 'generation barrier; renegotiation' },
  { section: 20, subject: 'desktop IPC, reloads, orphans', disposition: 'retained', cubmModule: 'hosts, version', correctionId: null, note: 'ipc-protocol axis; 2 leases per identity' },
  { section: 21, subject: 'diagnostics, traces, redaction', disposition: 'retained', cubmModule: 'outcomes', correctionId: null, note: 'safe detail; no-network default noted' },
  { section: 22, subject: 'cleanup, counters, early exits', disposition: 'retained', cubmModule: 'cleanup', correctionId: null, note: 'CLN-01/CLN-05 receipts' },
  { section: 23, subject: 'deterministic and live proof', disposition: 'retained', cubmModule: 'capabilities', correctionId: null, note: 'evidence levels; blocked stays blocked' },
  { section: 24, subject: 'absent/unsupported/unavailable, prohibitions', disposition: 'retained', cubmModule: 'capabilities, outcomes', correctionId: null, note: 'states never interchangeable' },
  { section: 25, subject: 'coverage ledger and validation', disposition: 'corrected', cubmModule: 'semantic-map', correctionId: 'AC-07', note: 'this ledger replaces checker-only coverage' },
] satisfies readonly SemanticMapping[];

export interface ApprovedCorrection {
  readonly id: string;
  readonly area: string;
  readonly fourXBehavior: string;
  readonly cubmDecision: string;
  readonly rationale: string;
}

export const APPROVED_CORRECTIONS: readonly ApprovedCorrection[] = [
  {
    id: 'AC-01',
    area: 'version axes',
    fourXBehavior: 'package release version stamps every backend identity alongside negotiated axes',
    cubmDecision: 'C-UBM freezes three axes: runtime (negotiated per attachment), contract revision (exact equality), build (observability only, never a handshake axis)',
    rationale: 'PKG-02: a binding that differs from core identity must fail before effects even when the npm version matches',
  },
  {
    id: 'AC-02',
    area: 'identity representation',
    fourXBehavior: 'opaque ids are string-brand intersections constructed with type assertions',
    cubmDecision: 'C-UBM identities are structural records with validated fields and explicit scope',
    rationale: 'the frozen wire form must carry its scope without type-system assertions, satisfying the no-silencing-cast rule',
  },
  {
    id: 'AC-03',
    area: 'numeric limits and 64-bit mapping',
    fourXBehavior: 'byte maxima are distributed across backend, manager, and public helpers with per-site enforcement',
    cubmDecision: 'C-UBM freezes one numeric table (65536 items, 4MiB stream, 4/16/64MiB aggregates, 524288 operation bytes, int32 timeout) and decimal-string u64/i64 wire mapping',
    rationale: 'DATA-02: values beyond the JS safe range must not narrow silently; one table prevents per-site drift',
  },
  {
    id: 'AC-04',
    area: 'recovery catalog shape',
    fourXBehavior: 'recovery actions carry payloads (permission names, byte maxima, millisecond delays)',
    cubmDecision: 'C-UBM freezes dispositions verbatim and action kinds only; payloads stay in the typed operation result',
    rationale: 'keeps the frozen contract free of host-resolved values while preserving the retry decision per code',
  },
  {
    id: 'AC-05',
    area: 'peripheral roles',
    fourXBehavior: '4.x is central-only; server scenarios exist only as future scenario ids',
    cubmDecision: 'C-UBM adds generic peripheral extension primitives (decls, CCCD reconciliation, atomic commit, targeted notify, response deadlines) with zero commercial or physiological fields',
    rationale: 'CON-UBM requires central and peripheral contracts together at the boundary with shared lifecycle primitives',
  },
  {
    id: 'AC-06',
    area: 'scan filter model',
    fourXBehavior: '4.x canonical scan queries support native pushdown plans (exact vs safe-superset) with residual digests',
    cubmDecision: 'C-UBM freezes only the admission boundary: canonical UUIDs, explicit duplicate/merge policies, and fail-closed unsupported-field rejection; pushdown planning stays backend-internal',
    rationale: 'the kernel contract must not bless a native filter reduction it cannot verify; residual equivalence is proven in the backend TCK, not assumed at admission',
  },
  {
    id: 'AC-07',
    area: 'coverage mechanism',
    fourXBehavior: '4.x validates the semantics document with a Markdown checker plus behavioral TCK fixtures',
    cubmDecision: 'C-UBM keeps the behavioral fixtures and adds this executable map: every section resolves to a module, corrections resolve to log entries, enforced by test',
    rationale: 'removing a category must fail the contract suite, not just document lint',
  },
] satisfies readonly ApprovedCorrection[];

export const MANDATORY_SCENARIOS: readonly string[] = [
  'OWN-01',
  'OWN-02',
  'CLN-01',
  'OPS-01',
  'OPS-02',
  'STR-01',
  'PKG-02',
] satisfies readonly string[];

export function correctionForMapping(mapping: SemanticMapping): ApprovedCorrection | null {
  if (mapping.correctionId === null) {
    return null;
  }
  for (const correction of APPROVED_CORRECTIONS) {
    if (correction.id === mapping.correctionId) {
      return correction;
    }
  }
  return null;
}
