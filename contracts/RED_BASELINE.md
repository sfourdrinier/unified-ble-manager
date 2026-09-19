# Contracts RED baseline (pre-fix)

Commit: d846bef9 (d846bef94697b97e1f72241e8dd0d180f6fe6b44)
Branch: codex/ubm5-contract-fixes
Date (UTC): 2026-09-15T22:19:33Z

Command: ./node_modules/.bin/jest --config contracts/jest.config.cjs

```

      342 |   test('canonical digit cap is frozen at 20', () => {
      343 |     const mod: any = require('../src/index');
    > 344 |     expect(readKeyViaJs(mod, 'MAX_DECIMAL_DIGITS')).toBe(20);
          |                                                     ^
      345 |   });
      346 |
      347 |   test('plus sign and whitespace are rejected', () => {

      at Object.toBe (__tests__/contract-fix-rulings.test.ts:344:53)

FAIL contracts/__tests__/semantic-map.test.ts
  ● 4.x semantic map › every approved correction is referenced bidirectionally (AC-02/04/05/06 included)

    correction AC-02 has no referencing semantic-map entry

      55 |       const referencing = SEMANTIC_MAP.filter(entry => entry.correctionId === correction.id);
      56 |       if (referencing.length === 0) {
    > 57 |         throw new Error(`correction ${correction.id} has no referencing semantic-map entry`);
         |               ^
      58 |       }
      59 |       for (const entry of referencing) {
      60 |         expect(correctionForMapping(entry)?.id).toBe(correction.id);

      at _loop2 (__tests__/semantic-map.test.ts:57:15)
      at Object._loop2 (__tests__/semantic-map.test.ts:54:50)

FAIL contracts/__tests__/fixtures-roundtrip.test.ts
  ● valid fixtures › every valid fixture executes through its validator by kind

    valid fixture capability-supported (capability) must execute: input.limitations is not iterable

      203 |         executeValidFixture(fixture);
      204 |       } catch (error) {
    > 205 |         throw new Error(
          |               ^
      206 |           `valid fixture ${fixture.name} (${fixture.kind}) must execute: ${error instanceof Error ? error.message : 'unknown'}`,
      207 |         );
      208 |       }

      at Object.<anonymous> (__tests__/fixtures-roundtrip.test.ts:205:15)

PASS contracts/__tests__/outcomes-errors.test.ts
PASS contracts/__tests__/identities-handles.test.ts
PASS contracts/__tests__/bounds-deadlines-numerics.test.ts
PASS contracts/__tests__/peripheral-roles.test.ts
PASS contracts/__tests__/central-gatt.test.ts
PASS contracts/__tests__/capabilities.test.ts
PASS contracts/__tests__/version-axes.test.ts
PASS contracts/__tests__/streams-accounting.test.ts
PASS contracts/__tests__/transitions.test.ts
PASS contracts/__tests__/cleanup-counters.test.ts
PASS contracts/__tests__/effects-races.test.ts
PASS contracts/__tests__/ownership-arbitration.test.ts

Test Suites: 3 failed, 12 passed, 15 total
Tests:       19 failed, 145 passed, 164 total
Snapshots:   0 total
Time:        0.857 s, estimated 1 s
Ran all test suites.
```

Failing suites: 3 (contract-fix-rulings, fixtures-roundtrip, semantic-map)
Failing tests: 19 (see list below)

-   ● 4.x semantic map › every approved correction is referenced bidirectionally (AC-02/04/05/06 included)
-   ● R11 failure contender and Failed terminal › a failure contender settles failed
-   ● R12 reserved-control like-with-like › byte capacity is compared against reserved bytes, not items
-   ● R12 reserved-control like-with-like › stream defaults carry reserved-control bytes and item counts
-   ● R14 canonical decimal form › canonical digit cap is frozen at 20
-   ● R1 effectiveMaxBytes ceiling › 524289 bytes are rejected even against a 1MiB declaration
-   ● R1 effectiveMaxBytes ceiling › a 1MiB backend max clamps to the frozen 524288 ceiling
-   ● R2 frozen tables › every shared table is frozen
-   ● R2 frozen tables › mutating a frozen table cannot change query results
-   ● R2 frozen tables › nested table contents are frozen
-   ● R3 capability limits copy-and-freeze › mutating the source limits after construction leaves the descriptor unchanged
-   ● R4 unknown version axes › unknown axis spans throw protocol.malformed
-   ● R5 unsupported filter fields › empty-string entries throw capability.unsupported
-   ● R5 unsupported filter fields › non-string entries throw capability.unsupported
-   ● R8 generic-shape allowlist › unlisted physiological keys are rejected
-   ● R9 GATT path invariants › characteristic uuid without occurrence throws
-   ● R9 GATT path invariants › descriptor without characteristic throws
-   ● R9 GATT path invariants › mismatched attachment/peer scope throws
-   ● valid fixtures › every valid fixture executes through its validator by kind
