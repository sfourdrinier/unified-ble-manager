# Slice C — documentation verification tests

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Execute **last**, after Slice A + D-docs. Master plan: `2026-08-19-rc1-review-response.md`.

**Goal:** Make the documentation bugs found in the 2026-08-19 review fail CI if they return.

**Architecture:** Three Jest files. Semantic regex rules over the teaching corpus. One executed finite recipe against `DeterministicTestBackend`. Link/anchor + version consistency. Fence labels on new examples; do not build a general Markdown compiler unless the recipe extract is cheap.

**Tech Stack:** Jest, `fs`, existing `unified-ble-manager/testing` deterministic backend, `package.json` version.

---

## Files

| File | Responsibility |
| --- | --- |
| `__tests__/Docs.semantic.test.js` | BLE/docs anti-patterns |
| `__tests__/Docs.recipes.test.js` | Execute the finite HRS helper journey on DeterministicTestBackend |
| `__tests__/Docs.consumer.test.js` | Keep wording/channel checks; add version + link existence |
| Teaching markdown | Optional `// @ubm-recipe finite-hrs` marker on the canonical fence |

---

### Task C1: Semantic rules (red, then green)

**Create:** `__tests__/Docs.semantic.test.js`

Teaching corpus (only these; do not scan ADR/FIX_TRACKER/GAPS):

```js
const teaching = [
  'README.md',
  'MIGRATION_4.0.md',
  'docs/GETTING_STARTED.md',
  'docs/TUTORIALS.md',
  'docs/HELPERS.md',
  'docs/WEB.md',
  'docs/NODE.md',
  'docs/ELECTRON.md',
  'docs/TAURI.md',
  'docs/PROFILES_AND_COMMANDS.md',
  'docs/EXPO_PLUGIN.md'
]
```

Rules (each is its own `test`):

1. No `batteryLevelSelector` (or `BATTERY_LEVEL`) used as the path of a `.write(` in the same fence.
2. No `readHeartRateMeasurement`, `readBloodPressureMeasurement`, `readTemperatureMeasurement` identifiers.
3. A fence that matches `localName` must not contain `duplicatePolicy: 'first'`.
4. No `Platform.OS === 'ios' ? 'apple' : 'android'`.
5. No `isBackgroundEnabled`.
6. No `Same manager contract on every host`.
7. Every `new AbortController()` in a finite recipe fence is either passed to a helper (`scanUntil`, `firstNotification`, `withConnection`, `withDiscoveredConnection`) or `.abort()` appears **before** a `for await` over the same controller’s work. (If this rule is too noisy, limit it to fences tagged `// @ubm-recipe`.)
8. Example READMEs that mention `AbortSignal` must not be the only claim — `example/src/services/BLEService/BLEService.ts` must contain `AbortController` or `signal:`.

```sh
pnpm exec jest __tests__/Docs.semantic.test.js --runInBand
```

After A+D-docs this should pass. If it fails, fix the **doc**, not the rule, unless the rule has a false positive on a clearly marked `pseudo` fence.

Allow an opt-out HTML comment on a fence: `<!-- ubm-fence: pseudo -->` immediately above it. Semantic rules skip `pseudo` fences.

---

### Task C2: Execute the finite README recipe

**Create:** `__tests__/Docs.recipes.test.js`

Do **not** eval README. Check in a recipe module that the README fence must match:

**Create:** `__tests__/docs-recipes/finite-hrs-journey.js`

The module exports `async function runFiniteHrsJourney(harness)` where `harness` provides:

- a `BleManager` from `createDeterministicTestBackend` / existing scenario factory
- a simulated HRS peripheral that **only** exposes Heart Rate Measurement (notify), no Battery, no Control Point
- virtual time / deadline that can complete

The function body uses the same helper sequence as README: `scanUntil` → `withConnection` → `discover` → `resolveCharacteristicPath(heartRateMeasurementSelector())` → `firstNotification` → `throwIfCleanupFailed(destroy)`.

Test:

1. `runFiniteHrsJourney` resolves and the manager has no leaked scans/connections (use `localResourceCounters()` or the deterministic leak helpers already used in manager tests).
2. README.md contains `// @ubm-recipe finite-hrs` in the first complete-loop fence, and that fence contains `scanUntil`, `withConnection`, `firstNotification`.
3. A second test: the same journey against a fixture **without** Battery/Control Point still succeeds (guards C3).

If constructing a full manager in Jest is awkward, copy the smallest pattern from `__tests__/scenarios/manager-scenarios.test.js` or `src/testing`.

```sh
pnpm exec jest __tests__/Docs.recipes.test.js --runInBand
```

Expected: PASS.

---

### Task C3: Links, anchors, and version

In `__tests__/Docs.consumer.test.js` (keep the file; add tests):

**Version:** every teaching file that mentions `4.0.0-rc.` must mention exactly `package.json`’s version. Do not hand-edit a generator. When the later version bump to `4.0.0-rc.1` happens, this test is what forces the banners to move together.

**Links:** for each teaching file, extract markdown links `[text](url)`:

- `http:` / `https:` — skip (do not network)
- `#anchor` — require a heading that slugifies to that anchor in the same file
- relative `*.md` — `fs.existsSync` from the file’s directory
- `SECURITY.md` / `SUPPORT.md` / `LICENSE` from README must exist

Heading slug: GitHub style (lowercase, spaces to `-`, drop punctuation) is good enough. If an existing heading cannot slug-match, fix the link.

**Filename banners:** keep the existing `<!-- README.md -->` first-line convention.

---

### Task C4: Packed-package fence typecheck (only if cheap)

If `node scripts/ci/pack-install-smoke.js` or an existing packed consumer already typechecks public imports, add one assertion that `createReactNativeBleManager`, `createNavigatorWebBleManager`, `createCoreBluetoothBleManager`, `adapterStates`, `throwIfCleanupFailed` exist on the **packed** package.

Do **not** start a new TypeScript project that extracts every markdown fence. That is the only C item allowed to shrink if it exceeds a few hours.

---

### Task C5: Slice C + rc.1 validation

```sh
pnpm exec jest __tests__/Docs.consumer.test.js __tests__/Docs.semantic.test.js __tests__/Docs.recipes.test.js --runInBand
pnpm test:package
pnpm test:plugin
pnpm lint
```

Expected: PASS.

Then stop. Version bump to `4.0.0-rc.1` is a **human-approved** follow-on from the master plan, not part of this slice’s code edits unless the user says to bump now.
