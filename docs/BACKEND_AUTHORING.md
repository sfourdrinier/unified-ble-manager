<!-- docs/BACKEND_AUTHORING.md -->

# Backend authoring

`unified-ble-manager/backend-sdk` is the only public backend-authoring entrypoint. Do not import `src/**`, first-party backend classes, or deterministic test internals.

Export one `unifiedBleBackend` value from a Node-loadable backend module. Create it with `createBackendAuthorDefinition()` using author-controlled package/backend/platform metadata, a `BackendTckFactory`, and the feature suites that implement the capability registry's required scenarios. The factory owns explicit adapter enumeration, the selected adapter, and a known-unlisted `staleSelection` used by the runner to prove stale-target rejection; it must negotiate before radio work and must not choose a convenient default adapter.

Register each capability with `createFeatureRegistry()`. A registration binds its namespaced ID, typed implementation, state, finite limits, limitations, evidence receipt, capability-schema range, and required TCK scenarios. `inspectBackendCapabilities()` marks every registry receipt as `author-declared`; it is safe report data, never a host-name guess, conformance receipt, or support claim.

Run `runBackendAuthorTck()` before publishing. A factory receives the exact scenario context and creates the backend, deterministic environment-input controller, and cleanup boundary; it cannot provide an executor, facts, proof scope, or a receipt. The controller drives advertisements, notifications, time, disconnects, and faults, but runner-owned scenario code alone exercises the public backend contract, decides whether required observations hold, and creates each public receipt with `verification: "runner-controlled"`. The same `TckScenarioController` contract is used by first-party and independently authored backends. Backend instances are single-use across scenarios and runs, so replay and concurrent substitution fail. Backend metadata and capability evidence cannot promote author-authored data into conformance. The public authoring runner produces deterministic conformance only; live-radio evidence uses the separate physical evidence workflow and cannot be selected by a backend factory. The complete generated source-derived IDs and state reference is [BACKEND_SDK_REFERENCE.md](generated/BACKEND_SDK_REFERENCE.md).

The Node CLI intentionally accepts only backends whose provider declares `node`, `desktop-native`, or `test`. Electron main and Tauri native providers use the framework-neutral `desktop-native` host kind; browser and React Native backends require their own host integration and cannot be driven from a Node shell.

## Scan planning contract

The additive PR9 backend contract exposes `ScanPlan`, `BackendScanPlanner`, and
the canonical normalized scan-query types. A plan retains the source query and
its `queryDigest` separately from the residual query and `residualQueryDigest`.
Native execution must be declared either `exact` for the complete source query
or `safe-superset`; residual evaluation remains the frozen PR4 matcher. Plan
limitations use structured predicate descriptions and finite redacted
diagnostic vocabulary, so raw advertisement bytes, peer identifiers, and host
payloads do not belong in plan diagnostics.

The public `ScanOptions.platform` contract is discriminated and validated
before radio work. A platform control that the selected host has not actually
implemented rejects with a typed capability error; it never silently no-ops.
Current first-party pushdown is limited to safe common service-UUID filters,
while names, byte predicates, RSSI, exclusions, and unavailable fields remain
in the canonical residual path. First-party planners must consume the shared
fixture corpus and prove native/residual equivalence before expanding pushdown.
Deterministic planner tests establish contract behavior; they do not promote a
backend support label or constitute physical-radio evidence.
