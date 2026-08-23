# PR10 review inventory

Target branch: `feat/expo-first-class-host`
Base: `a252b5a7d6e09147ccd7fa636facab6ad2996ea6`
Milestone: first-class Expo host, staged as PR10A–PR10D

This is a live disposition ledger. A finding is not closed by a test-only change; the current source, focused regression evidence, and the relevant package/native gate must agree.

| ID | Finding | Current evidence | Disposition | Required proof |
|---|---|---|---|---|
| F10-001 | Restoration identity was JavaScript-owned and used the obsolete newline/hex algorithm. | `src/public/host-identity.ts` now rejects JavaScript derivation; `src/react-native-app-manager.ts` requests native bootstrap identity using only the application restoration token/generation; Android/iOS controls now implement the native derivation boundary. | Addressed in source; Apple device build remains environment-gated. | Android compile/unit gate, Apple CI build, shared vectors, configuration mismatch and exactly-once adoption tests. |
| F10-002 | Expo plugin accepted the RC1 flat/five-field configuration and did not reconcile removals safely. | `plugin/src/expoPluginSchema.ts` is the canonical v2 validator; `plugin/src/withBLE.ts` stores app-facing restoration id/generation and removes stale managed keys. | Addressed in source; focused plugin tests 30/30 and plugin build pass. | Full package gate, prebuild snapshots, and native manifest/Info.plist verification. |
| F10-003 | Expo factory was a fail-closed stub with no readiness surface. | `src/expo.ts` delegates to RN, exposes additive readiness/permission/settings surfaces, and checks development-build/native configuration inputs before RN construction. | Addressed as a deterministic host surface; runtime permission/settings bridges remain explicit injection points and never fabricate grants. | Packed Expo consumer proof, native module runtime checks, and no fabricated permission claims. |
| F10-004 | No Strict-Mode-safe React provider/hooks subpath existed. | `src/react.ts`, `./react`, provider-scoped lease tests, and API report now exist. | Partially addressed: provider, adapter, and capability hooks are complete; plan-required discovery/connection/characteristic hooks remain open until their stable public observation contracts are implemented. | React lifecycle tests, API report, packed type/loadability proof, and remaining hook contract tests. |
| F10-005 | The Expo example did not consume the Expo factory or expose plan/readiness/diagnostics flows. | `example-expo` now uses `unified-ble-manager/expo` and v2 plugin configuration, but its screen-level plan/readiness/diagnostics work is not complete. | Partially addressed; assigned to PR10D. | Example typecheck, CNG prebuild, Android/iOS build, and packed tarball consumer proof. |
| F10-006 | The supplied `runner-public-scenarios.ts` explicit result annotations were suspected of causing artifact build failures. | Current source contains explicit `SecurityCancelPairingResult`/`SecurityPairResult` annotations for the cancellation scenario. | Verified; no logic change required. | Typecheck and `prepack` must remain green. |
| F10-007 | Android foreground-service/CDM and native restoration execution are not yet implemented by the current partial diff. | Plugin declarations and native bootstrap now exist; no runtime foreground-service lease, Companion Device Manager ceremony, or full restoration example is present. | Open; assigned to PR10D. | Native build/tests, lifecycle evidence, and explicit support limitations. |

## Current passing focused gates

- `pnpm test:plugin`: 30 tests passed.
- `pnpm build:plugin`: passed.
- `pnpm native-protocol:check`: passed.
- `pnpm lint`: passed after formatting and the restoration-factory const fix.
- `pnpm typecheck`: passed at the current partial TypeScript boundary.
- `pnpm test:package`: 153 suites / 1,465 tests passed.
- `pnpm test:native-protocol`: C++ protocol harness passed.
- `pnpm test:native-protocol:android`: Android protocol unit/build lane passed.
- `pnpm test:native-protocol:apple`: C++/Apple execution harness passed; no physical radio exercised.
- `pnpm release:artifacts:check`: passed.
- `pnpm performance:check`: passed (31 JS/core and 5 native-host measurements).
- Packed host, install-smoke, and G6A proofs: passed from tarballs.
- Focused Expo and React Native restoration Jest suites: passed before the aggregate package rebuild.

## Remaining release gates

The PR10 branch is not ready for review or RC4. Remaining gates include native Android/iOS implementation and tests, React subpath/provider, example and packed consumer proof, full package tests, `prepack`, artifact checks, evidence validation, native tests/builds, fresh adversarial reviews at the final SHA, GitHub Actions, and the required two-round PR review cycle.
