# C-UBM contract draft `C-UBM.0.1.2-DRAFT`

Status: **ACCEPTED at C-UBM.0.1.2-DRAFT** (CON-UBM, trackourhealth/bun-mono#1188; U1 accepted; amendment 0.1.2 accepted by lane orchestrator as additive-only and wire-compatible).
This directory is the exclusive U1 contract-freeze area on `codex/ubm5-contract-fixes`.

## What is frozen here

Versioned TypeScript contract types plus fail-closed runtime guards for the
executable generic kernel boundary:

| Module | Content |
|---|---|
| `src/version.ts` | runtime / contract / build version axes, handshake negotiation, PKG-02 gating |
| `src/identities.ts` | attachment tuples, peer identities, GATT occurrence paths, handles, generations, UUID/address canonicalization |
| `src/outcomes.ts` | full error-identity catalog, frozen profile-codec identities (0.1.2), terminal records, recovery dispositions, redacted platform detail |
| `src/bounds.ts` | numeric production limits, monotonic deadlines, u64/i64 decimal-string mappings |
| `src/capabilities.ts` | four-state descriptors, limitations, evidence receipts, built-in catalog |
| `src/hosts.ts` | host/lease/resource types, scan/connection arbitration (OWN-01), ownership transfer, authorization predicate |
| `src/effects.ts` | pure input/effect model, completion records, first-valid-contender arbitration (OPS-01/OPS-02/OWN-03), happens-before |
| `src/central.ts` | central API validation, mandatory write modes, long-write plans, controls façade |
| `src/peripheral.ts` | generic peripheral extension primitives (no commercial/physiological content) |
| `src/cleanup.ts` | cleanup receipts (CLN-01/CLN-05), resource counters, early-exit table |
| `src/streams.ts` | bounded-stream defaults and overflow accounting (STR-01) |
| `src/transitions.ts` | all nine §4 lifecycle machines plus contention rulings |
| `src/semantic-map.ts` | 4.x section map, approved-corrections log, mandatory scenario links |
| `src/freeze.ts` | shared deep-freeze helper for every frozen table |
| `src/fixtures/` | valid fixtures (incl. 64-bit boundaries) and invalid fixtures with frozen codes |

Disagreements with 4.x are logged as approved corrections (`AC-01`…`AC-07`)
against the protocol/contract — never as silent equality.

## Version axes

- **Runtime** (negotiated per attachment): `backend-contract`,
  `capability-schema`, `event-schema`, `trace-format`, plus `native-protocol`
  or `ipc-protocol` where a native/IPC boundary applies. Highest common value
  wins; disjoint ranges fail `protocol.incompatible` before any effect.
  Unknown axis names are rejected with `protocol.malformed` (R4).
- **Contract**: `C-UBM.0.1.2-DRAFT`, exact equality required.
- **Build** (e.g. npm `4.0.28`): observability only, never a handshake axis.

## Frozen decisions (0.1.1–0.1.2)

- **R1**: `effectiveMaxBytes` seeds with `MAX_OPERATION_BYTES`, so a 1 MiB
  backend declaration clamps to the frozen 524288 ceiling.
- **R2**: every shared table is deep-frozen via `freezeTable`
  (`src/freeze.ts`); frozenness is pinned by `__tests__/contract-fix-rulings.test.ts`.
- **R3**: capability `limits` are copied and frozen at construction; later
  caller mutation cannot reach the descriptor.
- **R4**: unknown version axes fail `protocol.malformed` (`isRuntimeAxis`).
- **R5**: any `unsupportedFilterFields` entry — string, empty string, or
  non-string — fails `capability.unsupported`.
- **R6**: every valid fixture executes through its validator by kind, and
  negotiated/terminal wire forms survive genuine serialize→revive round
  trips (`__tests__/fixtures-roundtrip.test.ts`).
- **R7**: `AC-02/04/05/06` are bidirectionally linked (§9→AC-02,
  §13/§15→AC-04, §10→AC-05, §6→AC-06).
- **R8**: generic-peripheral boundary is a fail-closed shape allowlist
  (`GENERIC_PERIPHERAL_ALLOWED_KEYS`): only listed generic keys admit;
  unlisted physiological/commercial keys (`spo2Sample`, `sleepStage`,
  `glucose`, `oura`, `dexcom`, `vo2max`, `blood-pressure`, …) fail
  `argument.invalid`. Allowlist growth requires a contract revision.
- **R9**: GATT paths enforce paired uuid↔occurrence, descriptor-requires-
  characteristic, and attachment/peer scope consistency
  (`peer.scope-mismatch` on scope divergence).
- **R11 (kernel D1)**: `failure` contender and `failed` terminal serve the
  existing `publish-failure → failed` edge.
- **R12 (kernel D3)**: streams compare like-with-like — byte budgets against
  `reservedControlBytes` (new `RESERVED_CONTROL_BYTES = 64` plus per-default
  `reservedControlCapacity`/`reservedControlBytes`); the mixed-units
  bytes-against-items comparison is removed. Data and control use separate
  item pools sharing one byte budget.
- **R13**: the error catalog is diffed verbatim against the 4.x oracle
  (`src/backend-contract/errors.ts`, 67/67); capability-state and peer-domain
  mappings use exhaustive switches that throw on future members.
- **R14**: canonical decimal form is `^-?[0-9]+$` with no plus sign, no
  whitespace, and at most `MAX_DECIMAL_DIGITS = 20` digits excluding an
  optional leading `-`. Enforced identically in TypeScript and the Rust
  mirror.
- **R15 (0.1.2 additive amendment)**: the four `profile.codec.*` SIG
  payload-codec identities (`truncated`, `malformed`, `reserved`,
  `invalid-value`) are frozen as `PROFILE_CODEC_ERROR_CODES` plus the
  `isProfileCodecErrorCode` guard, byte-identical to
  `src/profiles/errors.ts`. Payload-codec failures carry no `BleErrorDomain`
  and no recovery disposition, so they form their own table; the 67-code
  `BleErrorCode` oracle catalog is unchanged.

## Strict TypeScript rules in this directory

No `as any`, no double assertions, no non-null assertions, no `as const`,
no checker-silencing casts or directives. Inference by default, annotations
on exported boundaries, discriminated unions, exhaustive switches, and
`satisfies`-style frozen tables. Identities are structural records so the
wire form carries its scope with no assertion to construct. JS-reachable
invalid inputs are exercised through `any`-typed test helpers, never casts.

## Verification

```sh
pnpm contracts:typecheck
pnpm contracts:test
```

Positive examples compile and pass; negative examples compile and throw
their frozen codes at runtime. Wire round trips (JSON) are covered in
`__tests__/fixtures-roundtrip.test.ts`. The credible RED baseline for the
0.1.1 fix slice is recorded in `RED_BASELINE.md` (19 failing tests on
pre-fix code). CI runs both commands in the `contracts` job
(`.github/workflows/ci.yml`).

## Derivation gaps vs `src/`

1. **Planner pushdown** (`src/backend-contract/scan-planning.ts`): C-UBM
   freezes only the admission boundary; exact/safe-superset planning stays
   backend-internal (AC-06).
2. **Recovery payloads** (`src/backend-contract/recovery.ts`): dispositions
   frozen verbatim; action payloads stay in typed operation results (AC-04).
3. **Host transports** (native protocol v2 schema, IPC router, Electron
   preload): referenced by axis name only; byte-level wire schemas are not
   re-frozen here.
4. **Live-radio numbers**: budgets are the frozen 4.x production constants;
   measured physical-radio qualification remains PENDING-PREFLIGHT per the
   U0 baseline and is not claimed by this draft.
5. **Peripheral surface**: new generic primitives; no 4.x behavioral source
   exists to diff against (AC-05).
6. **Follow-up owner: kernel-wiring slice** (NOT implemented here):
   `CleanupRecord` wire codec (the `instanceof ContractError` check cannot
   cross JSON; needs a data-only encode/decode pair mirrored in Rust), plus
   the remaining LOW-3 notes (ownership-transfer authentication wording,
   handshake-operation validation order, `hasAbortSignal` truthiness,
   zero-length long-write segments, counter-ledger key retention,
   deterministic lease-string collisions, mixed-separator BLE addresses,
   `OWN-03` scenario-id definition). See the U1 review §LOW-3.
