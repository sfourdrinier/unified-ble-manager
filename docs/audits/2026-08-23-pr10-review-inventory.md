# PR10 review inventory

Target branch: `feat/expo-first-class-host`
Implementation receipts anchor: `f8c7a48f3f6c9063dda0c412f3ec0e76a8ebe99e` (historical implementation receipt; remediation commits are listed below).
Base: `a252b5a7d6e09147ccd7fa636facab6ad2996ea6`
Milestone: first-class Expo host, staged as PR10A–PR10D

This is a live disposition ledger. A finding is not closed by a test-only change; the current source, focused regression evidence, and the relevant package/native gate must agree.

| ID      | Finding                                                                                                                  | Current evidence                                                                                                                                                                                                                                                                      | Disposition                                                                                                                                 | Required proof                                                                                                                     |
| ------- | ------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| F10-001 | Restoration identity was JavaScript-owned and used the obsolete newline/hex algorithm.                                   | `src/public/host-identity.ts` now rejects JavaScript derivation; `src/react-native-app-manager.ts` requests native bootstrap identity using only the application restoration token/generation; Android/iOS controls now implement the native derivation boundary.                     | Addressed in source; Apple device build remains environment-gated.                                                                          | Android compile/unit gate, Apple CI build, shared vectors, configuration mismatch and exactly-once adoption tests.                 |
| F10-002 | Expo plugin accepted the RC1 flat/five-field configuration and did not reconcile removals safely.                        | `plugin/src/expoPluginSchema.ts` is the canonical v2 validator; `plugin/src/withBLE.ts` stores app-facing restoration id/generation and removes stale managed keys.                                                                                                                   | Addressed in source; focused plugin tests 30/30 and plugin build pass.                                                                      | Full package gate, prebuild snapshots, and native manifest/Info.plist verification.                                                |
| F10-003 | Expo factory was a fail-closed stub with no readiness surface.                                                           | `src/expo.ts` delegates to RN, exposes additive readiness/permission/settings surfaces, checks development-build/native configuration inputs, and fails permission requests closed without a trusted bridge.                                                                          | Addressed as a deterministic host surface; runtime permission/settings bridges remain explicit injection points and never fabricate grants. | Packed Expo consumer proof, native module runtime checks, and no fabricated permission claims.                                     |
| F10-004 | No Strict-Mode-safe React provider/hooks subpath existed.                                                                | `src/react.ts`, `./react`, provider-scoped lease tests, readiness/discovery/connection/characteristic hooks, and API report now exist.                                                                                                                                                | Addressed in the current hook surface; physical-radio behavior and hook integration remain host evidence, not deterministic proof.          | React lifecycle and hook teardown tests, API report, packed type/loadability proof.                                                |
| F10-005 | The Expo example did not consume the Expo factory or expose plan/readiness/diagnostics flows.                            | `example-expo` now uses `unified-ble-manager/expo`, v2 plugin configuration, dashboard readiness/plan/resource diagnostics, a dashboard path to the diagnostics/restoration screen, and redacted support-bundle projection. SDK 57 typecheck, CNG prebuild, and Android debug APK build pass. | Addressed for deterministic source/example surfaces; iOS/native and physical-radio qualification remains explicitly open. | Example typecheck, CNG prebuild, Android/iOS build, and packed tarball consumer proof. |
| F10-006 | The supplied `runner-public-scenarios.ts` explicit result annotations were suspected of causing artifact build failures. | Current source contains explicit `SecurityCancelPairingResult`/`SecurityPairResult` annotations for the cancellation scenario.                                                                                                                                                        | Verified; no logic change required.                                                                                                         | Typecheck and `prepack` must remain green.                                                                                         |
| F10-007 | Android foreground-service/CDM and native restoration execution are not yet implemented by the current partial diff.     | Android now has a metadata-gated `BlePlxForegroundService`, acknowledged promotion, persisted session-intent restart policy, ref-counted leases, a Companion Device Manager system-UI bridge returning `associated` metadata only, and the Expo no-argument native restoration claim. | Partially addressed; physical/native host evidence remains open.                                                                            | Android Gradle/unit gate, Expo association/restoration contract tests, Apple CI/full host build, and explicit support limitations. |

## Supplemental current disposition

F10-005 now has a dedicated public-API diagnostics/restoration screen, a
redacted support-bundle projection, SDK 57 typecheck, CNG prebuild, and an
Android debug APK build. F10-007 now has the Android CDM system-UI bridge and
the no-argument native restoration claim. Their remaining qualification is
Apple/full-host and physical-evidence scope, not missing deterministic source
surfaces.

## Frozen Codex round-1 findings at `a0ac1a0255553982719ae0bfc1dc4dec3e969f62`

These entries are pinned to the reviewed commit and remain open until current-source verification, focused regression evidence, and the relevant package/native gate agree. The inline comment IDs are retained so every disposition can be replied to and resolved on the PR.

| ID | Inline comment | Finding | Current disposition |
| --- | --- | --- | --- |
| F10-CODEX-001 | `3838605160` | Foreground-service reconciliation removes generic `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_CONNECTED_DEVICE`, and `POST_NOTIFICATIONS` declarations that may belong to other plugins when BLE background mode is disabled. | Addressed by `ebd5c3d`; ownership regression tests pass in the focused plugin lane. |
| F10-CODEX-002 | `3838605162` | The expanded native control surface still negotiates native protocol v2, so an older v2 development build may handshake and fail later on newly added methods. | Addressed by `000af50`: native record protocol remains v2, while `controlSurface: 2` is required before execution-runtime installation. |
| F10-CODEX-003 | `3838605165` | Lowercase-only application-ID validation rejects valid mixed-case iOS bundle identifiers returned unchanged by the native host. | Addressed by `e235528`; mixed-case regression and restoration suite pass. |
| F10-CODEX-004 | `3838605170` | The React scan dependency key omits `timeoutMs` and `AbortSignal` identity, allowing stale operation controls after rerender. | Addressed by `c80c0f9` and `bd6312f`; scan and characteristic operation-control regressions pass. |
| F10-CODEX-005 | `3838605175` | BLE hardware-feature reconciliation removes host-authored `android.hardware.bluetooth_le` declarations when `requiredHardware` is false or omitted. | Addressed by `ebd5c3d`; host-authored feature preservation regression passes. |

## Frozen follow-up review findings

The following findings came from the independent PR10 plan/documentation and adversarial reviews against the pre-remediation source. They remain tracked until current-source verification proves a fix or records a reasoned exclusion.

| ID | Finding | Current disposition |
| --- | --- | --- |
| F10-REVIEW-001 | The ledger overstated PR10 readiness and did not distinguish hosted CI, local gates, skipped Apple compile, and post-merge/RC4 gates. | Addressed in this ledger update; the new remediation SHA still requires a fresh hosted run, Apple label gate, merge, post-merge main CI, and RC4 admission. |
| F10-REVIEW-002 | Packed proof covered tarball exports/types but not a packed Expo app or EAS build. | Addressed by `3ee21a7`; claims are narrowed to packed export/type/loadability proof and source-tree CNG/Android evidence, with EAS/iOS/physical limits explicit. |
| F10-REVIEW-003 | The diagnostics screen was routed but lacked a normal dashboard navigation path. | Addressed by `51d4a7511730ad8cbbd18cbce6c76edf0e0fc96a`; recheck current example regression and typecheck. |
| F10-REVIEW-004 | Expo v2 onboarding was presented as current while the immutable package identity remained `4.0.0-rc.3`. | Addressed by `3ee21a7`; docs bind v2 onboarding to PR10/RC4 and do not mutate RC3. |
| F10-REVIEW-005 | The public `unified-ble-manager/react` subpath was absent from the main onboarding/API navigation and packed consumer proof. | Addressed by `3ee21a7`; public docs and packed React loadability/type coverage now exist. |
| F10-REVIEW-006 | `useCharacteristicValue` serialized options with `JSON.stringify`, losing `AbortSignal` identity and retaining stale operation controls. | Addressed by `bd6312f`; identity-aware regression coverage passes. |
| F10-REVIEW-007 | Android foreground-service invalidation could abandon cleanup ownership if registry close failed. | Addressed by `2fafb73`; Android failure-path tests and full JVM suite pass. |
| F10-REVIEW-008 | Expo native configuration digest validation was fail-open when one side was absent, and the direct factory path lacked the required pre-radio check. | Addressed by `c06f82c`; fail-closed and location-policy regressions pass. |
| F10-REVIEW-009 | Companion association could leave a stale pending promise after synchronous failure. | Addressed by `2fafb73`; association failure-path regression passes. |
| F10-REVIEW-010 | Expo readiness could report ready despite `legacyLocation: 'required'` because the model exposed only Bluetooth permission state. | Addressed by `c06f82c`; readiness returns truthful location-settings guidance without fabricating grants. |
| F10-REVIEW-011 | The Apple boundary inherited an Android-only effective-MTU operation without an explicit fail-closed capability guard. | Addressed by `ace039f`; Apple boundary regression and focused native suites pass. |
| F10-REVIEW-012 | The StrictMode regression test did not invoke the first cleanup, so it did not exercise the actual setup/cleanup sequence. | Addressed by `bd6312f`; test now executes setup → first cleanup → setup. |

## Post-final-review remediation wave

These findings were discovered by the fresh exact-SHA adversarial reviews after the `48eef20` receipt. The current source dispositions are pinned to the remediation commits below; the full package/native/host gates must still be rerun at the resulting tip before PR update.

| ID | Finding | Current disposition |
| --- | --- | --- |
| F10-POST-001 | Apple native handshake returned ABI 2 while the generated/JS boundary requires ABI 3; fatal teardown could be closed twice. | Addressed by `e54d754`; generated ABI response and idempotent close are covered by Apple guards/harness. |
| F10-POST-002 | Android JNI handshake allocated 12 version entries while validating/reading 14; invalidation could destroy a retryable runtime; association metadata could return placeholder ID 0. | Addressed by `15c2860`; focused Android tests, CMake, and Android build pass. |
| F10-POST-003 | Android bonding cancellation could report cancellation while the OS bond operation remained active. | Addressed by `4458423`; unsupported is reported on SDK 36 and native ownership remains until the terminal bond callback. |
| F10-POST-004 | React hook cleanup failures were swallowed after unmount. | Addressed by `544c781`; provider `onError`/development reporting and focused cleanup tests pass. |
| F10-POST-005 | Replacement providers could start a new manager before the previous provider released its native owner. | Addressed by `34bac49`; replacement-provider release-barrier regression passes. |
| F10-POST-006 | Expo plugin could delete host-authored iOS Bluetooth usage text when omitted. | Addressed by `32ba577`; marker-owned removal and host-preservation tests pass. |
| F10-POST-007 | Android pre-12 readiness could report ready without required legacy location guidance. | Addressed by `06a369f`; explicit legacy-location readiness tests pass. |
| F10-POST-008 | Apple reconnect/GATT paths did not enforce connection-generation ownership and allowed reconnect overlap during terminal disconnect. | Addressed by `15a3301` with bounded generation validation at native dispatch, disconnect-before-reconnect gating, and generation-bound GATT cache clearing. Apple harness is green; full simulator/host CI remains required. |

## Late review remediation wave

The subsequent exact-source native/consumer review found additional ownership and truthfulness defects. They are addressed by the current commits below; no item is closed by a source assertion alone—the final package/native/host gates remain required.

| ID | Finding | Current disposition |
| --- | --- | --- |
| F10-LATE-001 | Android foreground-service metadata did not match the native parser, and `requiredHardware: true` did not upgrade an existing optional BLE feature. | Addressed by `c5bdcfb`; plugin tests 37/37 and build/lint pass. |
| F10-LATE-002 | Android Service Changed cleared radio cache but was not wired to the dispatcher/public database invalidation path. | Addressed by `34e91e6`; focused dispatcher/event tests pass. |
| F10-LATE-003 | Apple could drop an immediate disconnect before JavaScript installed connection ownership. | Addressed by `a8ff38a`; generation-tagged pending disconnect admission and Apple harness regression pass. |
| F10-LATE-004 | Android invalidation could lose retryable dispatcher/native-handle ownership; service intent and lease retention were not failure-atomic; JNI values could truncate; pre-API-33 association could return ID 0. | Addressed by `9ceca10`; 36 Android JVM tests, ARM64/JNI build, and CMake/CTest pass. |
| F10-LATE-005 | Apple GATT occurrence parsing accepted trailing garbage such as `1junk`. | Addressed by `ef63ef8`; strict parser and Apple harness pass. |
| F10-LATE-006 | React hooks retained stale replacement state and silently discarded overflow. | Addressed by `6f93d68`; focused React tests and full package gate pass. |
| F10-LATE-007 | Apple module invalidation omitted native runtime close after execution/radio teardown. | Addressed by `0ed8e54`; idempotent runtime-close guard and Apple harness pass. |

## Latest local verification at `7fee776dd05d53012a17baae015db8159f07d8c8`

- `pnpm test:package`: 156 suites / 1,504 tests passed.
- `pnpm test:plugin`: 4 suites / 37 tests passed.
- `pnpm typecheck`, `pnpm lint`, `pnpm prepack`, `pnpm release:artifacts:check`, and `docs:check`: passed.
- Forbidden TypeScript smell scan and `git diff --check`: passed.
- C++ native protocol, Android full JVM, and Apple native protocol harnesses: passed; no physical BLE radio.
- Packed Expo/React/Tauri, pack-install smoke, and G6A deterministic tarball proofs: passed; physical radio/EAS not provided.
- Expo SDK 57 typecheck and full CNG Android debug build: passed; generated native directories/lockfile remain uncommitted.

## Latest source verification at `ef35e12d380a513406290b6ba9eda2abcd7792bd`

- `pnpm test:package`: 156 suites / 1,505 tests passed, including the Android Service Changed boundary path.
- Focused Android boundary/native suites, TypeScript, ESLint, and protocol checks passed; no physical-radio evidence.

## Final local verification receipts at `707e3c9`

- `pnpm typecheck`, `pnpm lint`, `pnpm test:plugin`: passed; plugin 35/35.
- Forbidden TypeScript smell scan over published `src`/`plugin`: no `as unknown`, `as any`, `as T`, or suppression directives.
- `pnpm test:package`: 156 suites / 1,497 tests passed.
- `pnpm prepack`: passed; 232 published source files, 1,940 source-derived artifacts, 16 plugin artifacts, and 125 entrypoint targets verified.
- `pnpm release:artifacts:check`: passed; 2 dependency-artifact files current.
- `node scripts/ci/packed-host-consumer-check.js`: passed for packed Expo/React/Tauri; `physicalRadio: not-provided`.
- `node scripts/ci/pack-install-smoke.js`: passed across all canonical consumer lanes.
- `node scripts/ci/g6a-packed-consumer-proof.js`: deterministic packed proof passed; hardware evidence absent.
- `pnpm performance:check`: passed with 31 JS/core and 5 native-host measurements.
- `pnpm test:native-protocol`: C++ host harness passed.
- `pnpm test:native-protocol:android`: Android protocol lane passed.
- Android library full JVM suite: passed; 29 actionable tasks.
- `pnpm test:native-protocol:apple`: C++/Apple parser/execution harness passed; no physical BLE radio.
- Apple Native Protocol Jest suite: 20/20 passed, including ABI, idempotent close, and generation-bound dispatch guards.
- Expo SDK 57 typecheck and full CNG Android debug build: passed; generated native directories and lockfile remain uncommitted.
- `git diff --check`: passed.

## Fresh local verification at `100a4dbec3324a5508622b615e49b040e246377e`

These receipts are from the post-remediation tip before the next hosted PR run. They are deterministic or host-build evidence only; no physical-radio or EAS claim is implied.

- `pnpm install --frozen-lockfile`: passed; lockfile unchanged.
- `pnpm validate:evidence`: passed for 3 evidence files.
- `pnpm typecheck`: passed.
- `pnpm lint`: passed with zero warnings.
- Forbidden TypeScript smell scan over `src`/`plugin`: passed with no forbidden casts or suppression directives.
- `pnpm test:package`: 155 suites / 1,485 tests passed.
- `pnpm test:plugin`: 4 suites / 34 tests passed.
- `pnpm prepack`: passed; 232 published source files, 1,940 source-derived artifacts, 8 plugin source files, 16 plugin artifacts, and 125 entrypoint targets verified.
- `pnpm release:artifacts:check`: passed; 2 dependency-artifact files current.
- `node scripts/ci/packed-host-consumer-check.js`: passed from the packed tarball for Expo/React/Tauri; `physicalRadio: not-provided`.
- `node scripts/ci/pack-install-smoke.js`: passed across canonical CJS/ESM, browser, native tooling, Electron, CLI, Web, BlueZ, third-party TCK, Bundler, Node16, and NodeNext.
- `node scripts/ci/g6a-packed-consumer-proof.js`: deterministic packed Node/Web/third-party TCK proof passed; hardware evidence absent.
- `pnpm performance:check`: passed with 31 JS/core and 5 native-host measurements.
- `pnpm test:native-protocol`: C++ host harness passed.
- `pnpm test:native-protocol:android`: Android protocol Gradle unit lane passed.
- `pnpm test:native-protocol:apple`: C++/Apple parser/execution harness passed; no physical BLE radio.
- Android library `:unified-ble-manager:testDebugUnitTest`: passed; 29 actionable tasks.
- Expo SDK 57 `tsc --noEmit`, clean CNG prebuild, and full Android debug APK build: passed; generated native directories remain ignored.
- `git diff --check`: passed.

## Previously retained receipts

- `pnpm test:plugin`: 30 tests passed before the review-remediation commits (the remediation lane separately passed 34 tests).
- `pnpm build:plugin`: passed.
- `pnpm native-protocol:check`: passed.
- `pnpm lint`: passed after formatting and the restoration-factory const fix.
- `pnpm typecheck`: passed at the current TypeScript boundary.
- `pnpm test:package`: 153 suites / 1,474 tests passed at the current SHA.
- `pnpm test:native-protocol`: C++ protocol harness passed.
- `pnpm test:native-protocol:android`: Android protocol unit/build lane passed.
- `pnpm test:native-protocol:apple`: C++/Apple execution harness passed; no physical radio exercised.
- `pnpm release:artifacts:check`: passed.
- `pnpm performance:check`: passed (31 JS/core and 5 native-host measurements).
- Packed host proof at the current SHA: Expo/Tauri tarball proof passed; `physicalRadio: not-provided`.
- Packed install smoke at the current SHA: canonical CJS/ESM, browser, native tooling, Electron, CLI, Web, BlueZ, third-party TCK, Bundler, Node16, and NodeNext passed.
- G6A at the current SHA: deterministic packed Node/Web/third-party TCK proof passed; hardware evidence remains explicitly absent.
- Expo SDK 57 typecheck, CNG prebuild, and Android debug APK build passed; generated native directories remain ignored and are not release artifacts.
- Focused Expo and React Native restoration Jest suites: passed before the aggregate package rebuild.

## Remaining release gates

The PR10 branch is not ready for merge or RC4 until the remediation tip has a
fresh full local gate run and hosted CI. Remaining gates are the Apple
full-module/Expo host build (the local Swift harness is not that proof), fresh
adversarial review of the post-fix SHA, the `ci:apple`-labeled GitHub Actions
run, the required two-round PR review cycle, merge, post-merge `main` CI, and
RC4 admission. Existing receipts remain historical until re-run against the
current tip; no physical-radio or EAS evidence is claimed.
