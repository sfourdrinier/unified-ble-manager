# C-UBM contract draft `C-UBM.0.1.0-DRAFT`

Status: **DRAFT — pending U1 acceptance** (CON-UBM, trackourhealth/bun-mono#1188).
This directory is the exclusive U1 contract-freeze area on `codex/ubm5-contracts`.

## What is frozen here

Versioned TypeScript contract types plus fail-closed runtime guards for the
executable generic kernel boundary:

| Module | Content |
|---|---|
| `src/version.ts` | runtime / contract / build version axes, handshake negotiation, PKG-02 gating |
| `src/identities.ts` | attachment tuples, peer identities, GATT occurrence paths, handles, generations, UUID/address canonicalization |
| `src/outcomes.ts` | full error-identity catalog, terminal records, recovery dispositions, redacted platform detail |
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
| `src/fixtures/` | valid fixtures (incl. 64-bit boundaries) and invalid fixtures with frozen codes |

Disagreements with 4.x are logged as approved corrections (`AC-01`…`AC-07`)
against the protocol/contract — never as silent equality.

## Version axes

- **Runtime** (negotiated per attachment): `backend-contract`,
  `capability-schema`, `event-schema`, `trace-format`, plus `native-protocol`
  or `ipc-protocol` where a native/IPC boundary applies. Highest common value
  wins; disjoint ranges fail `protocol.incompatible` before any effect.
- **Contract**: `C-UBM.0.1.0-DRAFT`, exact equality required.
- **Build** (e.g. npm `4.0.28`): observability only, never a handshake axis.

## Strict TypeScript rules in this directory

No `as any`, no double assertions, no non-null assertions, no `as const`,
no checker-silencing casts or directives. Inference by default, annotations
on exported boundaries, discriminated unions, exhaustive switches, and
`satisfies`-style frozen tables. Identities are structural records so the
wire form carries its scope with no assertion to construct.

## Verification

```sh
npx jest --config contracts/jest.config.cjs
./node_modules/.bin/tsc --noEmit -p contracts/tsconfig.json
```

Positive examples compile and pass; negative examples compile and throw
their frozen codes at runtime. Wire round trips (JSON) are covered in
`__tests__/fixtures-roundtrip.test.ts`.

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
