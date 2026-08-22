# Post-PR5 audit disposition, PR6 closure, and PR7 continuation

Source audit: `~/Downloads/unified-ble-manager-post-pr5-audit.md`
Audit snapshot: `bc4a347c496e994e97cec9da06e2c0a6223a72f8`
RC2 release snapshot: `main` / `ab331517083c5a580894adb3d79d075f299c9db5` / `v4.0.0-rc.2`
Historical PR6 audit source: `feat/4.0-pr6-audit-closure` / `dac701f3bef8213074c829f1dce8ce3a2f42df38`
Current PR7 implementation checkpoint: `feat/4.0-security-pairing` / `9201780` (implementation tip); documentation-only descendants from this commit

Execution authority for this continuation is the revised session instruction,
`docs/superpowers/plans/2026-08-20-next-12-prs.md`, this disposition ledger,
and `AGENTS.md`. The separate RC1-to-stable handoff is historical context and
is superseded where it conflicts with those authorities.

## Release decision

RC2 is an immutable, quarantined prerelease. It reached the tag workflow because no confirmed P0 required cancelling the already-running release operation, and the tag/package/CI/provenance gates are independent of the unresolved application defects. Publication recovery completed on workflow attempt 2 after npm registry attestation propagation; the GitHub Release, npm `latest` dist-tag, exact tag commit, tarball, and provenance source binding were independently read back successfully. RC2 remains quarantined: it must not be treated as the finished RC2 closure, stable-ready, or evidence that the normal user journey is production-safe.

The preserved PR6 implementation WIP remains in `stash@{0}` and is not popped or rewritten. No audit correction is added to the immutable RC2 tag.

## Disposition legend

| Disposition         | Meaning                                                                                           |
| ------------------- | ------------------------------------------------------------------------------------------------- |
| Confirmed / closure | Verified against current source; must be fixed and tested in the named PR6 closure slice.         |
| Confirmed / defer   | Real, but intentionally scheduled for a later slice with an explicit acceptance test.             |
| Fixed               | Current source or RC2 release correction addresses the audited claim; retain regression coverage. |
| Stale               | The audited code claim is no longer true; do not “fix” it again.                                  |

## Implementation findings

| Findings                                                                                  | Current disposition | Target                                                                                                                     |
| ----------------------------------------------------------------------------------------- | ------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| F-001, F-002, F-008, F-010, F-011, F-012, F-015, F-017, F-018, F-020, F-021               | Confirmed / closure | PR6A — public contract, options, lifecycle, identity                                                                       |
| F-003, F-004, F-005, F-006, F-007                                                  | Confirmed / closure | PR6B — host parity, examples, and protocol truth                                                                            |
| F-019                                                                                     | Confirmed / defer   | PR10 — native-authoritative React Native/Expo restoration; PR6B must preserve the fail-closed boundary and not derive a fallback |
| F-022, F-023, F-024, F-026, F-027, F-028, F-029, F-030, F-031, F-032, F-033, F-034, F-035 | Confirmed / closure | PR6C — IPC, Tauri, GATT, and fail-closed host lifecycle                                                                    |
| F-037, F-039, F-040, F-042                                                                | Confirmed / closure | PR6C — peer observation producers, provider capability truth, Web grant preservation                                       |
| F-025                                                                                     | Confirmed / closure | PR6C native lease-safe cancellation plus PR6E supervisor arbitration; four late-cancel/shared-peer race tests are required |
| F-009                                                                                     | Confirmed / closure | PR6A custom stream input validation and end-to-end stream-budget test                                                      |
| F-013                                                                                     | Confirmed / closure | PR6D public adapter/chooser/GATT/peer error-rehydration matrix                                                             |
| F-014                                                                                     | Fixed in PR7A       | Backend-authoritative public diagnostics, including backend-owned chooser resources and optional backend trace       |
| F-016                                                                                     | Confirmed / defer   | PR9 — provider discovery-kind descriptor matrix; Electron/Tauri bootstrap descriptors are covered in PR6C                 |
| F-036                                                                                     | Confirmed / closure | PR6C permutation-invariant normalized query digest tests                                                                   |
| F-038                                                                                     | Confirmed / closure | PR6C normalized public observation delivery tests                                                                          |
| F-041                                                                                     | Confirmed / closure | PR6C stalled Web `getDevices()` abort/deadline race tests                                                                  |

F-015 is partially fixed in the current PR6 WIP by widening public and Tauri helpers to accept `PeerReference`; closure requires the cross-facade regression tests. F-042 is fixed for the implemented Web origin-authorized capability; retain its Web capability regression test, while cross-host limitations remain under F-039. F-026 is distinct from F-031: descriptor metadata fabrication and rejection of unknown descriptor writes require separate tests.

## Documentation and developer-experience findings

| Findings                                                                           | Current disposition                  | Target                                                                                                                                                  |
| ---------------------------------------------------------------------------------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D-001, D-015                                                                       | Fixed in RC2 source/release metadata | Retain regression tests                                                                                                                                 |
| D-002, D-003, D-005, D-006, D-007, D-008, D-009, D-010, D-011, D-012 | Confirmed / closure                  | PR6D — current application-first docs and examples                                                                                                      |
| D-004, D-014                                                                       | Confirmed / defer                    | PR10 — Expo v2 schema and current native configuration; PR6D must label RC1 configuration as retired/deferred             |
| D-013                                                                              | Confirmed / defer                    | PR11 distribution/install closure; acceptance test is an independent Tauri consumer installing paired npm/crate artifacts without a `node_modules` path |
| D-016, D-017, D-018                                                                | Confirmed / closure                  | PR6D — generated API reports and executable packed recipes                                                                                              |
| F-043                                                                              | Confirmed / closure                  | PR6D — safe empty-directory documentation recipe                                                                                                        |

## Required PR6 sequence

PR6 is a milestone with explicit internal slices. The currently applied supervisor implementation is provisional WIP; it must not merge before A–D evidence is recorded. Use a stacked PR6A → PR6E checklist (or equivalent commits/PRs) so the supervisor implementation is not the first or only PR6 change.

1. **PR6A — public contract and primary journey:** fix `find` in every façade (including Tauri), chooser filter/grant semantics, custom stream input, normalized public observations, cleanup/error propagation, all accepted-option behavior, `PeerReference` target consistency, secure identity generation, and the exact README journey.
2. **PR6B — host truth:** migrate Node and Electron renderer façades, remove all `@ts-nocheck` from TypeScript under both `example/` and `example-expo/`, align protocol v2 axes, and preserve a fail-closed restoration boundary. Native-authoritative React Native/Expo restoration remains the explicit PR10 implementation gate; PR6B must not create a JavaScript fallback.
3. **PR6C — IPC/GATT/peer safety:** close invalidation, overflow, lease-safe late cancellation, Tauri duplicate/authorization/generation behavior, descriptor metadata and unknown-write semantics, GATT schema/topology, peer-reference producers, Web service-grant preservation, stalled `getDevices()` cancellation/deadline races, normalized observation delivery, provider discovery descriptors, and runtime capability truth.
4. **PR6D — documentation/evidence:** rewrite current guides, compile packed recipes, replace stale positive assertions with forbidden-token/current-recipe checks, generate/diff API reports, and run the public error-rehydration matrix.
5. **PR6E — connection intents and supervisor:** resume the preserved PR6 WIP only after A–D’s blocking tests are green. Add direct/when-available capability gating, deterministic retry/gate state, generation/session cleanup, and arbitration without hidden reconnect.

## RC3 entry gate

RC3 may be prepared only after all confirmed closure findings above have either:

- a current-code fix plus focused RED→GREEN regression coverage; or
- an explicit capability/host limitation that makes the public claim truthful, with a linked test and documentation.

No P1 from this ledger may remain silently deferred. P2 findings may remain deferred only with the target PR and a finding-specific acceptance test recorded here. Before RC3, additionally require all non-outdated P1 review findings to be resolved, the full package/native/TCK gates, packed-consumer checks, generated API-report diff, explicit deterministic-versus-physical evidence labels, and a fresh current-source re-audit of every deferred finding. The RC2 package remains immutable; RC3 is the first release candidate eligible to claim this closure work.

## Release-process requirement

Every deferred finding must become a linked blocking issue with one target PR, one acceptance test, and one owner. The current deferred register is:

| Finding | Issue | Owner | Target | Acceptance test / evidence |
| --- | --- | --- | --- | --- |
| F-019 | [#34](https://github.com/sfourdrinier/unified-ble-manager/issues/34) | `sfourdrinier` | PR10 | Native-authoritative restoration identity plus cross-language derivation fixtures and RN/Expo adoption regression |
| D-004, D-014 | [#35](https://github.com/sfourdrinier/unified-ble-manager/issues/35) | `sfourdrinier` | PR10 | Expo schema-v2 validator, generated config fixture, and clean `example-expo` prebuild/config inspection |
| F-016 | [#37](https://github.com/sfourdrinier/unified-ble-manager/issues/37) | `sfourdrinier` | PR9 | Provider discovery-kind descriptor matrix and per-provider runtime capability regression |
| D-013 | [#38](https://github.com/sfourdrinier/unified-ble-manager/issues/38) | `sfourdrinier` | PR11 | Independent Tauri consumer installs paired npm/crate artifacts and builds without a `node_modules` Cargo path |

Each RC checkpoint must re-audit this register against current source; stale or unsupported findings must be explicitly closed rather than silently disappearing.

## PR7A current checkpoint

At commit `1de583bcd92495bc9150a33b80512b2dd9a81539`, PR7A has landed:

- typed security state/result/ceremony contracts and root/backend-sdk exports;
- manager-admitted public security operations with peer-reference resolution,
  capability gates, bounded custom challenges, explicit cancellation, and
  unsupported Apple/Web/IPC behavior;
- deterministic pairing state, watch events, duplicate arbitration, timeout and
  cancellation settlement, custom-agent validation, and destroy cleanup;
- backend-authoritative public diagnostics, with a regression proving a live
  backend chooser counter and backend trace are not replaced by core-local zeroes;
- the opt-in protected-GATT security helper and `platform.security` recovery
  actions;
- a canonical deterministic security TCK scenario, generated API reports,
  backend SDK scenario reference, and packed third-party security-unsupported
  assertions.

Android security is now tracked as the PR7C1 working-tree slice; Windows pairing/unpairing, BlueZ system pairing,
and trusted-host Electron/Tauri security scopes remain PR7C4 work. No native
support closure claim is made by PR7A, PR7C1, or PR7C4. The local packed smoke reached `npm pack` but was
blocked by npm's local exit-handler failure; hosted supported-Node CI remains a
required gate before PR7 can merge.

### PR7C1 Android working-tree checkpoint

The implementation source is clean through `d21cf6f` on
`feat/4.0-security-pairing`; this checkpoint is a documentation-only
descendant, and the exact branch tip is verified separately. It contains the additive Native Protocol v2
security command/result/event schema, generated C++/Kotlin/Swift/TypeScript
bindings, Android public-API bond state and `createBond` handling, the RN
boundary/provider adapter, and deterministic boundary/TCK coverage.

The current compile-SDK-36 Android artifact intentionally advertises only
state/pair support. Pair cancellation is not registered because the supported
public `cancelBondProcess` API is newer than this artifact; no hidden reflection
or `removeBond` API is shipped. Generic Android unpair remains explicitly
unsupported. Abort/timeout uses a cleanup-only command to release library
ownership; the OS ceremony may still finish later. Security events are enabled
only after a security-aware command and attachment-validated before delivery,
so an older or stale Native Protocol v2 peer cannot receive an unknown/stale event.

Verified in this checkpoint: `pnpm typecheck`, `pnpm native-protocol:check`,
`pnpm test:native-protocol`, the Android boundary suite (10 tests), the
React-Native Android vertical slice (30 tests), the first-party deterministic
backend TCK registry (including the Android security suite), and the full
package gate (123 suites, 1,146 tests). The Android Gradle
lane remains unverified locally because the example checkout lacks
`example/node_modules/@react-native/gradle-plugin`; hosted Android compile/JVM
evidence and physical-radio evidence remain open. This is not an RC3 or merge
claim.

## PR7C implementation inventory

The remaining host work is intentionally atomic by boundary:

| Slice | Required owned surfaces | Required proof before integration |
| --- | --- | --- |
| Android bond/system pairing | protocol schema and ABI manifest/version, generated C++/Kotlin/Swift/TypeScript bindings, Android radio bond receiver and public-API calls, Kotlin dispatcher, RN boundary/provider adapter | malformed-record rejection, bond-state ordering, terminal pair result, cancellation/timeout/destroy cleanup, Android unit/compile lanes, deterministic-versus-physical labels |
| Windows system pairing/unpair | typed WinRT boundary, native addon DeviceInformation pairing methods, backend state/result mapping, cancellation and cached-peer invalidation | PairAsync/UnpairAsync terminal tests, HRESULT/user-cancel mapping, native ABI/build lanes, no custom-agent claim without PairingRequested transport |
| BlueZ system pairing/unpair | D-Bus Device1 Pair/CancelPairing, Adapter1 RemoveDevice, Paired property observation, peer-path/cache invalidation | in-memory D-Bus lifecycle tests, system-agent limitation, remove-device destructive semantics, capability/TCK binding |
| Electron/Tauri security scope | trusted-host command projection, distinct state/pair/cancel/unpair/custom permission decisions, renderer reload cleanup | authorization matrix, malformed/stale generation rejection, no generic invoke permission widening, native plugin/IPC CI |

No slice may advertise a supported or limited security capability until its
owned boundary and required TCK/evidence proof land together. Apple and Web
remain explicit unsupported/implicit system-managed surfaces unless a public
API provides a truthful measurement; Web origin `forget()` is not generic
unpairing.

### BlueZ PR7C3 checkpoint

The BlueZ system-mediated subset now satisfies that boundary at commit
`b1be210a0bd4e2ad76bf0e98d82425a143f654e2` plus the current TCK integration:

- `security:state`, `security:pair`, `security:cancel-pairing`, and
  `security:unpair` are registered only after the D-Bus implementation exists;
- `Device1.Pair`, `CancelPairing`, `Adapter1.RemoveDevice`, and `Paired`
  property observation have lifecycle tests;
- removal invalidates the cached peer path and closes security watchers;
- the first-party BlueZ TCK executes the security scenario with four passing
  capability bindings;
- `security:custom-ceremony` remains unregistered and Agent1 remains an
  explicit unsupported exclusion;
- encryption, authentication, and Secure Connections are reported unsupported
  because BlueZ `Device1` properties do not provide those measurements here.

Android, WinRT, and trusted-host IPC security remain separate PR7C slices.

### WinRT PR7C2 internal checkpoint

Commit `25e36224af5f2d6ebab13396653f5f4e83189c6b` adds an optional typed WinRT
security boundary and adapter with state/watch, system-only pairing arbitration,
cancellation, unpair projection, and cleanup tests. It deliberately does not
register security capabilities or claim Windows support until the native
Node-API addon implements the same methods/events and the WinRT first-party TCK
and ABI/build evidence are added. Existing native artifacts therefore remain
fail-closed and unsupported.

Commit `4cc6e1aa9efddf44cfbb32844f375784d906aaf2` adds the corresponding native
Node-API source methods and strict loader/CI surface checks for
`securityState`, `pair`, `cancelPairing`, `unpair`, and `onSecurityState`.
Windows compilation and runtime/TCK evidence are still required; source-level
tests on macOS do not qualify the native artifact.

The deterministic WinRT first-party TCK now exercises the same system-only
security adapter with measured bond state, terminal pairing, cancellation,
unpair, watcher cleanup, and explicit custom-ceremony unsupported semantics.
The native Windows compile/ABI lane remains the qualification gate for turning
this deterministic binding into a Windows capability claim.

### PR7C4 trusted-host security-scope checkpoint

The PR7C4 trusted-host boundary is implemented in the clean commits
`ed6e05a` (Tauri permission definitions), `0965fff` (Electron), `96a6078`
(custom-ceremony authorization), and `45a22e5` (Tauri custom-ceremony
authorization), with later implementation descendants and documentation-only
descendants. Tauri now exposes distinct command scopes for state, system pair,
pair cancellation, unpair, and custom ceremony; unpair and custom ceremony are
not part of the default permission. Rust enforces the injected scope before
dispatch and ignores renderer-supplied scope fields. Electron snapshots
main-process-derived security permissions at renderer bootstrap and rejects
scope mutation or unauthorized security commands before router handlers run.

Neither host promotes security capability support: the Tauri btleplug
dispatcher and Electron IPC public adapter still report the security backend as
unsupported until concrete native security methods, DTO routing, and matching
TCK/evidence exist. Custom Electron ceremonies remain rejected rather than
downgraded because the current data-only IPC has no challenge/response wire
protocol.

Evidence: Tauri Rust tests (21 on the local macOS target; the Linux-only
authorization test is included by the hosted Linux target), focused
Electron/Tauri tests previously passed, full package gate at the current
candidate passed (123 suites, 1,175 tests), lint/typecheck, clippy, native protocol,
plugin, docs/API, evidence, artifact, diff, and forbidden-assertion-smell
gates passed locally at the prior implementation checkpoint. Hosted CI,
Android/Windows native qualification, and
physical-radio evidence remain open; no RC3 or merge claim is made.

### PR7 review-finding disposition at the current implementation tip

The first Codex review was triggered against an earlier PR tip, but every
reported finding was re-verified against the current source and is tracked here
by its exact fix and regression proof:

| Review finding | Disposition | Exact proof |
| --- | --- | --- |
| Copilot Android concurrent-pair arbitration | Fixed | `32d47b4`; Android boundary regression test rejects the second same-peer pair with `ownership.denied`. |
| Fresh adversarial Android pre-native-submission cancellation race | Fixed | `85e137a`; internal abort is checked after Android security-state preflight and before `securityPair`; abort/deadline tests prove no late submission. |
| Fresh adversarial Android admission after close | Fixed | `85e137a`; closed security backend rejects state/watch/pair/cancel/unpair admissions with `lifecycle.destroyed`. |
| Codex `3835119248` BlueZ cancellation while `Device1.Pair` is pending | Fixed | `a571fb3`; dispatcher cancellation hook, one-shot `CancelPairing`, deferred-pair abort/deadline tests. |
| Fresh adversarial BlueZ peer/reset invalidation | Fixed | `466c181`; peer removal and backend generation reset cancel active pairing and terminate security streams while retaining physical ownership. |
| Fresh adversarial BlueZ cancellation-hook retirement | Fixed | `356d538`; dispatcher idle/physical settlement waits for the memoized cancellation hook before boundary teardown. |
| Codex `3835119251` WinRT deadline after `PairAsync` dispatch | Fixed | `06c9eee`; shared `WinRtOperationDispatcher` deadline/physical-retirement path and deadline cancellation test. |
| WinRT TCK ownership gap after explicit cancellation | Fixed | `03349c4`; native completion retires security ownership before the next idempotent pair, with WinRT TCK and focused regression coverage. |
| Fresh adversarial WinRT state/unpair cancellation and deadline admission | Fixed | `42ba4eb`; state and unpair now use the shared dispatcher and reject pre-aborted operations before native dispatch. |
| Fresh adversarial WinRT adapter-loss security lifecycle | Fixed | `42ba4eb`; adapter loss closes watches, retires listener generations, quarantines stale events, and reopens only after cleanup. |
| Codex `3835119257` public security-watch async error rehydration | Fixed | `d28f96c`; deferred source/iterator/teardown error tests and public error bridge. |
| Fresh adversarial public watch failure cleanup | Fixed | `52f3a35`; next/done/return failure paths close the source exactly once and preserve aggregate teardown errors. |
| Codex `3835119260` BlueZ closed-watch ownership | Fixed | `a571fb3`; stream close unregisters immediately and the no-later-event retention test passes. |
| Fresh adversarial BlueZ initial-state registration rollback | Fixed | `466c181`; failed initial state reads remove the stream registration before rethrowing. |
| Codex `3835119263` Tauri custom-ceremony permission bypass | Fixed | `45a22e5`; Rust permission mapping requires both `Pair` and `CustomCeremony` for serialized custom ceremonies. |
| Fresh adversarial Electron object-form custom ceremony | Fixed | `52f3a35`; all non-system ceremony payload forms require `security:custom-ceremony`. |
| Final adversarial WinRT physical ownership ordering | Fixed | `3c68577`; security ownership is retired only from `physicalCompletion`, while native-result paths await physical retirement before reuse. |
| Final adversarial Android/WinRT failed-watch registry retention | Fixed | `3c68577`; initial security-state failures close and unregister the affected watch. |
| Final adversarial BlueZ pre-start cancellation race | Fixed | `3c68577`; queued operations check caller terminal state before invoking the physical operation. |
| Final TypeScript smell: Web Bluetooth non-null assertion | Fixed | `700d0e9`; explicit runtime guard replaces the non-null assertion; smell scan remains clean. |
| Final adversarial unsupported protection-level requests | Fixed | `e53629c`; Android, BlueZ, and WinRT reject non-default protection rather than silently ignoring it. |
| Final adversarial WinRT current-link security overclaim | Fixed | `e53629c`; native WinRT pairing projection leaves encryption/authentication unsupported because pairing protection is not a current-link measurement. |
| Final adversarial Android state and BlueZ unpair cancellation/deadline gaps | Fixed | `e53629c`; Android state uses a cancellation/deadline race and BlueZ unpair uses the operation dispatcher. |
| Final adversarial overflow-plus-cleanup error loss | Fixed | `e53629c`; public watch overflow preserves both the overflow and cleanup failures. |
| Second Codex P1 Android peer-ID translation | Fixed | `3481c27` and `95ab287`; CoreBluetooth-owned mappings now translate Android public IDs to native IDs and events back, including TCK fixture wiring. |
| Second Codex P1 WinRT peer-ID translation | Fixed | `3481c27` and `95ab287`; WinRT security receives the backend-owned bidirectional resolver and never publishes native IDs. |
| Second Codex P2 WinRT cancelPairing operation controls | Fixed | `3481c27` and `9201780`; composite cancellation is dispatcher-tracked, honors pre-abort/deadline admission, and retains both cancellation and pairing physical ownership. |

The exact candidate now has a complete green hosted PR40 CI run; it still
requires the final exact-tip adversarial review, the second Codex review round,
and the remaining native/evidence gates below before PR7 can merge.

### PR7E evidence disposition

| Evidence area | Current disposition | Authoritative proof or blocker |
| --- | --- | --- |
| TypeScript/API/docs/generated support | Proven | `pnpm docs:check`; 23 API entrypoints; Tauri permission schema/reference and Electron/Tauri security docs are committed. |
| Package/prepack/deterministic packed consumer | Proven locally | `pnpm test:package` passed 123 suites / 1,175 tests, including prepack and G6A packed-consumer coverage. |
| Native protocol and Tauri native scope | Proven locally | `pnpm native-protocol:check`, `pnpm test:native-protocol`, `cargo test --manifest-path native/tauri/Cargo.toml` (21 tests on local macOS; Linux adds the platform-specific authorization test), and clippy `-D warnings` passed. |
| Desktop IPC security capability truth | Proven locally | Electron/Tauri scope tests and `ElectronPublicManager` regression ensure unsupported native security is not promoted through IPC bootstrap. |
| Raw npm pack-install smoke | Blocked locally | `node scripts/ci/pack-install-smoke.js` reaches local npm packing but hangs/encounters the npm exit-handler failure; hosted supported-Node proof is required. |
| Android Gradle/JVM and physical Android | Blocked/open | Local lane is blocked before compilation by missing `example/node_modules/@react-native/gradle-plugin`; hosted Android compile/JVM and physical-radio evidence remain required. |
| WinRT native ABI/runtime and physical desktop | Blocked/open | macOS cannot qualify the Windows native artifact; hosted Windows compile/ABI/runtime and physical evidence remain required. |
| Hosted CI and release qualification | Awaiting exact code tip `9201780` | Hosted PR40 run `32557364881` is green for the prior semantic candidate; the second-Codex peer-ID/cancellation fixes require a new exact-tip hosted run. Apple compile checks are skipped by the workflow’s opt-in policy. |

PR7E is therefore an explicit evidence checkpoint, not a closure claim. Its
remaining local packed-smoke, physical-radio, and later-release distribution
blockers must be re-audited at the exact PR7 merge candidate before opening the
final PR7 review cycle.

## PR6 current checkpoint

This branch has now verified the following PR6 slices against current source:

- the shared IPC public façade is used by both Electron and Tauri, with no duplicated public IPC/GATT policy;
- Electron’s authenticated router projects the public manager through scan, adapter state, connect, RSSI, discovery, characteristic/descriptor read/write, notification, and cleanup flows;
- cleanup receipts, write receipts, notification metadata, GATT properties/access, host-projected leases, discovery descriptors, and native-rich scan observations have focused regression coverage;
- Electron renderer documentation, composition examples, package-surface fixtures, and the reviewed renderer API report describe the public factory;
- Tauri late-cancellation cleanup and lifecycle terminal delivery retain retry ownership rather than abandoning failed cleanup.

The exact current PR6 source has green local package evidence (117 suites,
1,120 tests), plugin/native/lint/typecheck, evidence, documentation/API,
artifact, smell, and diff gates, plus green hosted Node22/Node24, Windows and
macOS, Tauri, React Native Android, and Expo CNG CI lanes. The final exact-SHA
adversarial reviews found no actionable source or lifecycle findings, and all
first- and second-round review findings are replied to and resolved on PR #39.
Native-authoritative React Native/Expo restoration is an explicit PR10
deferral tracked by [#34](https://github.com/sfourdrinier/unified-ble-manager/issues/34); PR6 must not claim it is
implemented. RC3 remains gated for after PR8, and this ledger does not promote
PR6 to a release claim by itself.

## PR8 current checkpoint

The current PR8 branch is `feat/4.0-link-controls` at `9041c4f`, with the
readiness implementation committed in `0626c51`, the latest readiness/docs
follow-ups in its descendants, and the cumulative package gate reverified
afterward.

| Area | Current evidence | Disposition |
| --- | --- | --- |
| Public controls and API surface | Required `BleConnection.controls`, required `rediscoverGatt`, generation-bound observations, canonical root exports, and reviewed root API report | Fixed and locally verified |
| Android priority | Versioned Native Protocol v2 command/result, Android `BluetoothGatt.requestConnectionPriority` through the per-device serial queue, accepted/rejected result without observed-parameter overclaim | Android-only, limited/deterministic; hosted Android and physical evidence remain open |
| Android PHY | Versioned Native Protocol v2 read/request results, API-26+ `readPhy`/`setPreferredPhy`, callback mismatch/stale-generation rejection, public accepted-vs-observed result | Android-only, limited/deterministic; hosted Android and physical evidence remain open |
| CoreBluetooth readiness | `canSendWriteWithoutResponse`, `peripheralIsReady(toSendWriteWithoutResponse:)`, native ordinal/generation, bounded stream, disconnect/destroy cleanup | Direct Node/Electron-main only, limited/deterministic; other hosts remain unsupported |
| Scheduler and recovery | Per-connection bounded round-robin lanes, queue overflow, service-change cancellation, reasoned rediscovery, uncertain-write preservation, hidden-refresh source guard | Fixed with focused TDD/TCK coverage |
| TCK and docs | PR8 closure scenarios, Android PHY/priority truth, explicit unsupported parameters/subrate/other-host readiness truth, migration/semantics guidance | Deterministic evidence only; no physical-radio claim |
| Local package gate | `pnpm test:package`: 133 suites / 1,210 tests; docs/API, native protocol, artifact, and smell checks included | Green locally |
| Native/plugin gates | CoreBluetooth macOS Node-API build, native protocol host, plugin 36/36, release artifacts, typecheck/lint | Green locally; Android/Windows hosted qualification remains required |
| Packed consumer gate | Local npm pack smoke still encounters npm `Exit handler never called!` | Hosted supported-Node proof required |
| Performance baselines | Deterministic baseline `unified-ble-pr8-deterministic-performance-v1`: 23 measurements in the focused 3-payload test and 31 measurements / 15 categories in default `performance:check`, including all ten PR8 IDs, bounded cleanup and ownership metadata | Deterministic baseline fixed; live/native comparison remains a PR12 gate |
| Remaining contract gaps | `writeWhenReady` helper and effective-MTU observation are not yet implemented; parameter/subrate remain unsupported because no truthful pinned API is wired | Blocking PR8 acceptance until implemented or explicitly reconciled with the plan |
| Review/release | No PR8 GitHub PR, Copilot/Codex review rounds, merge, or RC3 tag yet | Blocking; RC2 remains immutable |
