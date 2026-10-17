<!-- docs/review/FIRETV_READINESS_FIX.md -->

# Unified BLE Manager 5.0.1 / Fire TV readiness

Status: **candidate evidence current as of 2026-10-09; not published or release-qualified**.

This record supersedes the earlier readiness note. It is not an early-scan-only
receipt, and it does not invent a final-approval gate. The user authorized the
5.0.1 candidate and publication work; the local GitHub login is invalid, but the authorized lx5090wifi host has a
working existing GitHub login. Canonical CI and trusted publishing are accessible
through that host; release qualification is still required.

## Candidate and source state

- Source: `/home/stephane/src-trackourhealth/ubm-firetv-readiness`.
- Changelog authority: `CHANGELOG.md`, `[5.0.1]` dated 2026-10-08.
- Candidate: `/tmp/ubm501-candidate-scan-expiry/unified-ble-manager-5.0.1.tgz`.
- Candidate SHA-256 (verified):
  `132c3353531f5bb3e8059af892a7d29ec2a56af4fbdc3a7763ca749d264bf798`.
- The tarball header is `package/package.json`, version `5.0.1`; it contains
  the current source and native paths. This replaces references to the older
  `/tmp/ubm501-candidate-final` archive.
- There is no frozen commit, push, tag, npm publication, or PR yet. The
  temporary consumer override points at this candidate and must be restored to
  the published dependency before final consumer delivery.

## 5.0.1 fixes covered by the candidate

- Expo Android readiness reads native location-services and legacy-location
  permission observations on every probe. Android API 24–30 with a non-`none`
  legacy policy therefore fails closed until both measurements are true;
  `none` requires the documented native rebuild. Android API 31+ with explicit
  `required` requests Bluetooth plus coarse/fine location. The implementation
  is in `src/expo.ts` and
  `android/src/main/java/com/sfourdrinier/unifiedblemanager/expo/UnifiedBleExpoRuntimeModule.java`.
- React Native cleanup retains failed subscription/database/connection
  ownership and retry debt after disconnect or native-link release. Discovery
  and rediscovery are fenced while retained cleanup is unresolved, rather than
  handing out a stale database or dispatching a replacement too early. The
  current authority is
  `src/backends/reactnative/react-native-rust-core-manager.ts` and
  `src/backends/reactnative/react-native-rust-core-provider.ts`.
- React Native RustCore scan observations carry the canonical origin peer
  reference, avoiding an extra peer-directory lookup before a consumer can
  retain a discovered device (`src/backends/reactnative/`).
- Android compatibility deprecations use the current Gradle DSL while keeping
  legacy OS routes and modern presence/chooser behavior explicit.
- The Linux Polar H10 simulator has hardened indication lifecycle and receipt
  handling. Its corrected PSFTP routing models the explicit-negative `.51`
  response path; it does not claim recording/file capability. The simulator is
  a test tool (`tool/h10-sim/`) and is excluded from the published package.

## Verification and evidence

- Fresh full-package receipt `/tmp/ubm501-scan-expiry-package-tests2.log`:
  **438 suites and 5,731 tests passed**. The preceding run's obsolete Gradle
  assertion was corrected against the assignment DSL; its focused regression
  also passed (7 tests).
- Fresh local receipts: `validate:evidence` passed (3 files), `test:plugin`
  passed (67 tests), and `lint` passed with zero ESLint warnings plus TypeScript.
  Applicable Android/Linux native inputs are fresh; dependency artifact checks
  passed (2 files). See `/tmp/ubm501-final-{validate-evidence,plugin-tests,lint,native-status,artifacts-check}.log`.
- Linux cannot compile Apple native code or establish the full five-desktop
  prebuild matrix. These remain hosted release gates, and the source has not
  yet been frozen to a release commit.
- The recent consumer receipt is
  `/home/stephane/src-trackourhealth/bun-mono-ubm5/ai-logs/2026-10-08.ubm5-hearts-upgrade.md`.
  It records the candidate hash above, package/prepack checks, and current
  Fire TV observations. APK45 is installed and streams all four modalities with engine metrics;
  sustained uninstrumented qualification remains open.

## Fire TV / consumer evidence

APK45 is the current installed consumer build, with the optimized engine and original Skia background. APK34 provided the earlier
manual-disconnect and reconnect evidence. With the simulator, the log
records HR, RR, ECG at 130 Hz, and ACC at 200 Hz / ±2 g; the BiometricEngine
ECG metric was `ok` with matching source/device ownership. A loopback link drop
also exercised the tracked Fire TV BlueZ client: terminal notification outcomes,
source retirement, automatic reconnect, and renewed streams were observed.
Manual disconnect/reconnect suppression was then observed over several minutes,
and an explicit Connect restarted the simulator connection. This is stronger
than the former scan-only note, but it is consumer integration evidence, not a
published-package release qualification.

The H10 stream was observed earlier, but the simulator does not establish
clinical physiology. Fixed simulated RR naturally produces zero variability,
and simulator/transport fidelity is not a patient or real-sensor calibration
claim. Temporary profiling diagnostics have been removed from source; the matching
production Android artifacts are rebuilding. Sustained uninstrumented
performance remains open. The exact FrameEvents missing-frame log has been
root-caused to Android OS logspam removed upstream in July 2023; old Fire TV
Android 11 still emits it. This is separate from the fixed Skia invalid
updateTexImage producer bug. See the [official Android change](https://android.googlesource.com/platform/frameworks/native/+/0a321db33ed5b3831d39ff36a3b14c48434a849e%5E%21/). No OS modification or log suppression is claimed.

## Remaining gates and scope

- Publish 5.0.1 through canonical trusted publishing after frozen-source qualification and hosted CI; no manual bypass is claimed or attempted.
- Re-run the appropriate clean release/consumer qualification from one frozen
  commit, including the packed-candidate path and applicable CI prebuilds.
- Apple TV, Google TV, mobile, and Tauri hardware are later qualification work.
  Apple compilation and the complete five-desktop matrix are host-CI work, not
  Linux evidence.
- Restore the actual published consumer dependency after candidate testing and
  remove temporary diagnostics/overrides before final qualification.

This document records current evidence and limits; it does not approve the
release or claim that the remaining gates have passed.

## 2026-10-09 final local scan-expiry update

The scan lifetime correction passed 13 focused tests, full package tests (438
suites / 5,731 tests), lint/types, prepack, evidence validation and generated
dependency-artifact checks. Plugin tests passed 67 tests. The candidate above
was packed after prepack without modifying package sources. Its canonical
packed-consumer gate failed because this Linux-only candidate lacks the four
other required desktop prebuilds. This is a release assembly limitation, not a
passing packed-consumer receipt; the hosted matrix must build the complete set.
No placeholder binaries or gate bypass was used.

Two similarly named simulator scan rows were traced to two actual adapters,
not duplicate identity: local hci0 and remote lx5090 hci1. The old remote
simulator was replaced with the current SHA-verified binary and the local
simulator stopped. The current remote simulator streams HR/IBI/ECG/ACC to
APK45 with the engine running. APK45 does not include the newest scan-expiry
JavaScript correction or engine coverage correction; final rebuilt-consumer
qualification remains pending.

## Clean-checkout API report qualification correction

The clean remote Linux preflight of `d5d70625` exposed compiler-assigned
unique-symbol IDs in API reports: IDs changed between TypeScript programs
without any public API change. The generator now uses the computed property
expression and, for custom unique symbols, the declaring module and symbol
name. Built-in symbols are identified from their default-library declaration,
so a user-defined object named `Symbol` cannot hide a custom symbol identity.
Six regressions cover compiler noise, same-name symbols in distinct modules,
alias rebinding, a shadowed `Symbol`, standard symbols and report parsing.
Focused tests and `pnpm docs:check` passed. This invalidates the earlier source
freeze for documentation qualification; fresh clean preflight is required.
Runtime sources and native artifact identities are unchanged by this fix.

The first clean rerun passed all 5,737 assertions but Jest discovered the four
type-only generator fixtures as executable suites. Fixtures were moved to
the existing ignored test-helper directory; no tests were disabled or Jest
configuration weakened. The six generator regressions passed after relocation.
Fresh frozen-source qualification follows this packaging correction.
