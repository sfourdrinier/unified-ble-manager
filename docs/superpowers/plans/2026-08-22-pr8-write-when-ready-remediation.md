# PR8 writeWhenReady Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the frozen PR8 `writeWhenReady` path bounded, generation-safe, capacity-accounted, cleanup-reporting, capability-truthful, and accurately documented.

**Architecture:** Keep readiness as a FIFO-head `CoreOperationAdmission`. Bound only acquisition waiting with the existing operation-admission helper, while retaining ownership of the raw backend-open promise so a source that resolves after cancellation is closed exactly once. Return readiness cleanup records through the coordinator and merge retained failures into connection/manager cleanup instead of tracing them away. Revalidate both the database path and readiness stream state at the dispatch boundary.

**Tech Stack:** TypeScript core/manager/public façade, Jest JavaScript contract fixtures, committed Markdown API reports, pnpm scripts.

---

### Task 1: Add focused RED regressions

**Files:**
- Modify: `__tests__/core/write-when-ready.test.js`
- Modify: `__tests__/core/operation-coordinator.test.js`
- Modify: `__tests__/public-write-when-ready.test.js`
- Modify: `__tests__/package-surface/fixtures/public-surface.ts`
- Modify: `__tests__/Docs.pr8.test.js`

- [x] **Step 1: Add the smallest failing tests**

Cover these independent behaviors: a never-settling readiness open rejects on abort/deadline and does not hold coordinator/manager teardown; a late-resolving watch is closed; a failed watch close becomes a retained `CleanupFailure`; an admitting operation consumes queue capacity; database/service-change invalidation and readiness-stream termination prevent native dispatch; `unavailable` remains `capability.unavailable`; `writeWhenReady` accepts only operation controls; the public surface compiles with those controls; and the root API report names the method with its exact options.

- [x] **Step 2: Run each focused test and verify the expected RED failures**

Run:

```sh
pnpm exec jest --config jest.config.js --runInBand \
  __tests__/core/write-when-ready.test.js \
  __tests__/core/operation-coordinator.test.js \
  __tests__/public-write-when-ready.test.js \
  __tests__/Docs.pr8.test.js \
  __tests__/PackageSurface4.test.js
```

Expected: the newly added assertions fail for the current unbounded open, admission-capacity count, cleanup propagation, stale-path/capability mapping, public option type, or API-report text; existing tests remain otherwise executable.

### Task 2: Implement ownership-safe core/coordinator behavior

**Files:**
- Modify: `src/core/core-characteristic-operations.ts`
- Modify: `src/core/operation-coordinator.ts`
- Modify: `src/core/unified-ble-core.ts`
- Modify: `src/core/unified-ble-core-helpers.ts` only if the existing helper needs a typed cleanup-admission companion

- [x] **Step 1: Bound readiness acquisition with `awaitWithOperationAdmission`**

Pass the operation options and a monotonic clock into the readiness admission. Start the raw backend-open promise once, attach both fulfillment and rejection handlers immediately, and let bounded admission waiting settle on abort/deadline. If the raw source resolves after the operation has closed, call its `close()` once and retain its cleanup result; never leave the late source unowned.

- [x] **Step 2: Preserve readiness cleanup records**

Make the admission close path return a `CleanupRecord`. Convert rejected/invalid close results to a cleanup failure, preserve every `CleanupFailure` in the coordinator, expose a read-and-clear or snapshot operation for lifecycle owners, and keep normal admission tests compatible with a released/no-failure cleanup result.

- [x] **Step 3: Revalidate before dispatch**

At `isReady()`/dispatch recheck, assert the generation-bound database/path and reject a closed/terminated readiness source. Ensure service-change, stale database/path, and externally closed readiness streams fail before `backend.gatt.write` is called.

- [x] **Step 4: Count admitting operations in capacity**

Include `admitting` operations in the per-connection capacity scan while continuing to exclude the active dispatched operation, preserving the existing quota error and payload ownership behavior.

### Task 3: Propagate cleanup failures and public contract truth

**Files:**
- Modify: `src/core/unified-ble-core.ts`
- Modify: `src/manager/ble-manager.ts`
- Modify: `src/public/gatt.ts`
- Modify: `__tests__/public-write-when-ready.test.js`
- Modify: `__tests__/package-surface/fixtures/public-surface.ts`

- [x] **Step 1: Merge coordinator cleanup failures into release records**

After connection queue cancellation and during manager-owned resource teardown, wait only for the relevant admission drain, collect retained readiness failures, and return `release-failed` with those failures. Do not mark the manager/core released when a readiness watch close failed.

- [x] **Step 2: Preserve unavailable versus unsupported**

Map a registered readiness descriptor with state `unavailable` to `capability.unavailable`; map absent/unsupported registration or an absent backend seam to `capability.unsupported` according to the existing capability contract.

- [x] **Step 3: Narrow the public method options**

Change `GattCharacteristic.writeWhenReady` to accept the exact operation-control shape (`OperationOptions`, `signal` and `timeoutMs`) and stop accepting `response`/write-mode options. Update runtime tests and the package-surface fixture to use the truthful type.

### Task 4: Update authoritative docs/API report and verify GREEN

**Files:**
- Modify: `README.md`
- Modify: `docs/UNIFIED_SEMANTICS.md`
- Modify: `etc/api/root.api.md`
- Modify: `__tests__/Docs.pr8.test.js`

- [x] **Step 1: Document bounded admission and cleanup semantics**

State that the helper is write-without-response only, takes operation controls, is generation/path bound, rejects unavailable distinctly, closes readiness ownership on cancellation/teardown, and reports cleanup failures.

- [x] **Step 2: Add the exact root API report method and docs assertion**

Add `GattCharacteristic.writeWhenReady(value: Uint8Array, options?: OperationOptions): Promise<GattWriteReceipt>` to the reviewed root report and assert that exact signature in the focused docs regression.

- [x] **Step 3: Run focused GREEN checks**

Run the focused Jest command from Task 1 and confirm all new and existing tests pass. Then run `pnpm typecheck`, `pnpm lint`, `pnpm docs:check`, and `git diff --check`.

### Task 5: Final validation and handoff

**Files:**
- Inspect only the owned-file diff and existing unrelated work. The native readiness ingress is part of this remediation's explicitly isolated native slice.

- [x] **Step 1: Run the requested canonical checks proportionate to the change**

`pnpm validate:evidence`, `pnpm test:package` (142 suites / 1,366 tests),
`pnpm test:plugin` (36 tests), `pnpm prepack`,
`pnpm release:artifacts:check`, performance, protocol, Tauri, and the
CoreBluetooth addon build are green. The local packed-consumer smoke remains
environment-blocked by npm's `Exit handler never called!`; hosted supported-
Node proof is now green in hosted run `32592655162`, including generic
pack/install and G6A. The local npm `Exit handler never called!` remains an
environment-specific limitation; `077d797` updates the stale validator to the
current 43-fact contract. Hosted classic/Expo Android compilation also passed
in that run; no physical-radio claim is made.

The later PR8 follow-up slices are committed separately: CoreBluetooth
connection-scoped readiness cleanup in `986dfda`, bounded quarantine cleanup
and cancellable drain waiters in `0635889`, and the deterministic teardown
fast-path regression fix in `0940b8b`. The hosted run above predates those
commits and must be rerun at the final pushed SHA before merge.

The native-operation quarantine and adapter-loss follow-up slices are also
committed: physical settlement is `2edb3ea`, and adapter-loss fail-closed
cleanup is `ed83e8a`. The generated documentation/public-boundary slice is
`95494e1`; local package and focused CoreBluetooth gates are green. The later
runtime hardening commits are `762c00f` (canonical physical settlement),
`947f479` (changed-observation preservation), and `5f29080` (CoreBluetooth
connection idle barrier), plus the explicit Expo ESM import fix in `14850ff` and
the final BlueZ/WinRT barriers in `c864d9d`/`016d269`.
The hosted run remains bound to `046b764` until the final branch tip is pushed.
The serialized runtime hardening commits are `4224c0a` (CoreBluetooth late
native ownership and adapter-loss retry), `1fd13f2` (BlueZ confirmation and
batch-cleanup retry), `b7aa1f8` (CoreBluetooth post-start teardown),
`56f5492` (BlueZ logical ownership), and `4297f7b` (WinRT bounded teardown, settlement, and
callback guards). The final source tip for the current local gate is
`99bf789`; the final host-boundary follow-ups are `59cdb33`/`f31bca6`/`b27b03b` (CoreBluetooth),
`fc1c667`/`2814d2c`/`812ba53`/`b853040` (BlueZ), `8fa029e`/`bbdf891`/`2e2acb4`/`99bf789` (WinRT), and `1bd5060`/`2c03dd7`/`8e5f4db` (release docs/tests).
The evidence-refresh commits are `8e5f4db` plus the current source tip `99bf789`; the full package gate is green at 142
suites / 1,366 tests at the current source tip.

- [x] **Step 2: Verify scope and report exact evidence**

The native/electron/CoreBluetooth addon diff is limited to the bounded
readiness ingress and focused guards. Exact commit identities and changed-file
scope are recorded in the branch history and audit ledger; local green gates,
environment-gated limitations, and the absence of hosted/physical evidence are
explicitly retained. The final CoreBluetooth native-cleanup timeout finding
remains a blocking gate before the serialized push/merge step.
