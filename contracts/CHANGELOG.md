# C-UBM contracts changelog

## `C-UBM.0.1.1-DRAFT` (U1 review follow-up, `codex/ubm5-contract-fixes`)

Contract-fix slice for UBM 5.0 (trackourhealth/bun-mono#1188): independent
review verdict ACCEPT-WITH-FIXES plus Rust mirror sync. Test-first with a
credible RED baseline (`RED_BASELINE.md`: 19 failing tests on pre-fix code).

- **R1**: `effectiveMaxBytes` seeds with `MAX_OPERATION_BYTES`; 1 MiB
  declarations clamp to the 524288 ceiling.
- **R2**: deep-freeze every shared table via `src/freeze.ts#freezeTable`
  plus frozenness tests (top-level and nested, mutation attempt).
- **R3**: capability `limits` are copied and frozen at construction.
- **R4**: unknown version axes rejected (`isRuntimeAxis`, `RUNTIME_AXES`,
  guarded in `makeVersionSpan`/`negotiateVersionSpan`/
  `assertNegotiatedWithinOffer`).
- **R5**: any `unsupportedFilterFields` entry rejected regardless of type.
- **R6**: every valid fixture executes by kind; negotiated versions and
  terminal records use genuine serialize→revive round trips. Fixed
  `capability-supported` (full descriptor), `peripheral-service`
  (`characteristics: []`), and `stream-limits-*` (`reservedControlBytes`)
  fixtures.
- **R7**: bidirectionally linked `AC-02/04/05/06` (§9→AC-02, §13/§15→AC-04,
  §10→AC-05, §6→AC-06) plus a test that every correction is referenced.
- **R8**: MEDIUM-6 decision — generic-shape allowlist
  (`GENERIC_PERIPHERAL_ALLOWED_KEYS`, fail-closed; replaces the
  `PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS` denylist). Unlisted physiological
  keys rejected; growth requires a contract revision.
- **R9**: paired uuid↔occurrence validation, descriptor-requires-
  characteristic, and attachment/peer scope consistency
  (`peer.scope-mismatch`).
- **R10**: `contracts:test` + `contracts:typecheck` scripts, CI `contracts`
  job, and `RED_BASELINE.md`.
- **R11 (kernel D1)**: `failure` contender + `failed` terminal serving the
  existing `publish-failure → failed` edge (TS + Rust, incl. kernel
  `cause_for` → `platform.failure`).
- **R12 (kernel D3)**: reserved-control byte capacity (`RESERVED_CONTROL_BYTES
  = 64` + per-default fields) with like-with-like byte comparison; the
  mixed-units check is removed (TS + Rust).
- **R13**: oracle-diff test vs `src/backend-contract/errors.ts` (67/67) and
  exhaustive switches for capability state and peer domain.
- **R14**: canonical decimal form documented + enforced + mirrored in Rust
  (`MAX_DECIMAL_DIGITS = 20`, no plus/whitespace).
- **Rust mirror**: `contracts.rs`/`streams.rs`/`ownership.rs` synced to all
  fixed semantics (revision `0.1.1-DRAFT`, `RuntimeAxis::{as,from}_str`,
  `GattPath`/`GattPathParams`, generic allowlist, decimal cap, stream
  budgets, failure terminal); 60/60 `cargo test` green.
- **Gap-logged (not implemented)**: `CleanupRecord` wire codec + remaining
  LOW-3 notes → follow-up owner kernel-wiring slice (see README §6).

## `C-UBM.0.1.0-DRAFT` (initial U1 draft)

Initial frozen draft reviewed as ACCEPT-WITH-FIXES (10-item fix list).
Superseded by `0.1.1-DRAFT` above.
