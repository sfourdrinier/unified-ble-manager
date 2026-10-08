# Unified BLE Manager — PR #251, second complete review

**Disposition: request changes. The implementation is not complete and equivalent public behavior is not yet established across supported platforms.**

Reviewed head: `b80542f39144949738b04dc7d19b4a7ee52cc8c6`. Previous reviewed head: `86c9cd2ff86e22e410c95880fab59aca82f99bb1`. Base: `f702f3c116b2f45fba64cfceb94d3916cadc9baf`. PR: [https://github.com/sfourdrinier/unified-ble-manager/pull/251](https://github.com/sfourdrinier/unified-ble-manager/pull/251). Current CI: [run 37558288591](https://github.com/sfourdrinier/unified-ble-manager/actions/runs/37558288591). Review date: 7 October 2026 UTC.

The PR has 16 commits, 188 changed files and +11,027/−1,547 lines. Since the previous review there are three commits, 148 changed files and +8,700/−1,344 lines. During this review, the branch advanced from 65be89d4 to b80542f3; the final 24-file delta (+612/−183) was separately reviewed and affected reproductions were rerun. This report evaluates the actual pinned source and current gates, including the whole current central/GATT capability surface where needed to judge completion. It does not accept the commit message “Make the rc.21 review commit pass CI” as evidence of passing CI.

## Decision and interpretation

There are **33 open action items: 2 P1, 30 P2 and 1 P3**. Two additional items were fixed by the commit that landed during this review (R2-10 and R2-21), so 35 IDs are tracked overall. These are not 33 newly introduced runtime bugs. They include new regressions, residual findings, inherited supported-feature omissions, one combined CI gate finding and a Bun qualification gap. Every item states its origin and evidence level. Related cross-platform directory omissions are grouped as one work item; the three RN write-admission symptoms likewise share one underlying finding.

The two immediate priorities are the remaining CI failures and the Linux accepted-Pair ownership race. The missing NAPI parameter forwarding is also a direct completion blocker for the new Windows feature. The recurring implementation problem is that a new API is wired through a successful path without closing its complete contract: admission, byte ownership, event ordering, terminal errors, resource retirement, host projection and capability truth. Reusing a shared contract should eliminate platform divergence instead of adding independent helper logic for each host.

Substantial fixes deserve credit: WinRT descriptor enumeration, hard delivery requirements, first CCCD selection, normal structured GATT errors, mobile dirty-scope fairness, Apple when-available connect, real readiness plumbing and truthful Apple MTU handling all improved. Several original Linux cases now have positive controls. The closure table below identifies these precisely.

## What the last commit closed during this review

The final commit was reviewed in a separate checkout. **R2-10 (IPC source-error projection) and R2-21 (Tauri service restrictions) are closed at this head.** The actual public IPC reproduction now receives `adapter.powered-off` as a public `BleError` and preserves its operation. Tauri now obtains the access fact from the service row and emits the correct restriction, including when a characteristic row precedes it. The old restriction gap must not be repeated as an open finding.

The seven API reports are regenerated and pass their checker. The packed Linux Bun gate now executes successfully, as does the source-mode smoke. Classic RN and Expo CNG Android builds both pass. The new IPC admission compensation retains failed unsubscribe cleanup for retry, and IPC scan evidence now ages from a real receipt clock. The three directly changed JS suites pass locally: 66 tests across 3 suites.

The final commit does not fix the common cache’s per-field expiry, identity, list-merging or retention defects; those reproductions still demonstrate all four. Five RN readiness cases and both TS parameter cases also still reproduce. Linux production ownership inputs are byte-identical, and the fingerprinted C evidence transfers unchanged. R2-34/35 were confirmed while triaging the now-executing CI suite; they are real missing implementation paths, not merely outdated fixtures. Separately, the parameter TCK helper still accepts only unsupported/unavailable descriptors and never invokes an operation; those two TCK failures require actual callable-state coverage and are not runtime proof of R2-34.

## Priority index

| ID | Priority | Area | Current finding |
|---|---|---|---|
| R2-01 | P1 | all package consumers | The latest canonical package tests and Rust gates still fail |
| R2-02 | P1 | Linux maintained BlueZ daemon | An accepted external Pair can lose its link when UBM releases its last lease |
| R2-03 | P2 | Linux Rust client and BlueZ daemon | Protected logical release forgets the owner of deferred cleanup and token retirement |
| R2-04 | P2 | Linux maintained BlueZ daemon | Deferred AcquireWrite/AcquireNotify socket errors leave a durable interest |
| R2-05 | P2 | Linux maintained BlueZ daemon | Foreign one-shot GATT operations started before the first lease are unprotected |
| R2-06 | P2 | Linux maintained BlueZ daemon | Physical loss before any lease exists leaves arrival records and stale ownership |
| R2-07 | P2 | React Native Apple | RN Apple writeWhenReady violates payload ownership, FIFO admission and invalidation |
| R2-08 | P2 | React Native Apple | RN readiness streams lose or hang on native source failure |
| R2-09 | P2 | Electron renderer on macOS, Tauri on macOS | Electron/Tauri public characteristics still cannot writeWhenReady |
| R2-11 | P2 | Tauri | Tauri watch quota is consumed permanently by successfully released handles |
| R2-12 | P2 | shared scan paths, WinRT native ingress | Scan accumulation still misses split service lists and native ingress drops needed evidence |
| R2-13 | P2 | shared advertisement and IPC scan evidence | Unrelated scan packets refresh old field evidence beyond its expiry window |
| R2-14 | P2 | shared address-bearing scan paths, WinRT ingress | Scan evidence merges distinct public/random peer identities with identical address bits |
| R2-15 | P2 | long-running shared scans, WinRT admitted addresses | The scan evidence cache retains inactive peers without a capacity or expiry sweep |
| R2-16 | P2 | Windows desktop provider, hosts using that provider | A delayed initial parameter probe is emitted after a newer live event |
| R2-17 | P2 | public parameter streams, Windows native parameter source | Parameter events accept invalid measurements that the snapshot API rejects |
| R2-18 | P2 | Windows connection parameter reads | Lost-link parameter reads use a different error vocabulary from other link controls |
| R2-19 | P2 | WinRT; compare Android | Desktop code drops a soft delivery preference that WinRT can honor |
| R2-20 | P2 | WinRT | Initial WinRT discovery and compound subscription rollback still flatten native errors |
| R2-22 | P2 | desktop native, React Native, Tauri | Native GATT graphs fabricate primary services and empty inclusion relationships |
| R2-23 | P2 | Windows 11 build 22000+ | Windows preferred connection presets remain unreachable from requestPriority |
| R2-24 | P2 | Windows before build 22000 | Parameter capability registration ignores the actual Windows API floor |
| R2-25 | P2 | WinRT parameter event source | Native parameter-event failures and early queue loss remain invisible to consumers |
| R2-26 | P2 | Linux, Windows, Android, iOS | Peer directories still omit OS-supported known or system-connected retrieval |
| R2-27 | P2 | Linux Node/Bun Rust route | Optional BlueZ acquired-FD GATT transports are explicitly left unimplemented |
| R2-28 | P2 | Android SDK 36.1+ | Android subrate requests remain an unconditional stub on capable authorized hosts |
| R2-29 | P2 | Android API 36 events; 36.1 snapshot | Android security discards encryption observations that public APIs provide |
| R2-30 | P2 | Android scan | Android rejects public batching and PHY scan options instead of implementing them |
| R2-31 | P2 | Windows scan | Windows scan controls are a placeholder despite supported WinRT options |
| R2-32 | P2 | Bun Linux, Bun macOS, Bun Windows | Passing Linux Bun gates still omit the real packed desktop integration and two operating systems |
| R2-33 | P3 | Windows scan planner | Windows scan planning still says connectability is unavailable after the native fix |
| R2-34 | P2 | Windows Node, Windows Bun, Electron on Windows | The NAPI dispatch wrapper never forwards connection_parameters |
| R2-35 | P2 | React Native internal/advanced manager | The exported RN internal connection and bridge shape omit the new parameter methods |

## Current findings

### R2-01 — P1: The latest canonical package tests and Rust gates still fail

**Origin:** current gate failures, including defects exposed after the documentation repair. **Scope:** all package consumers. **Evidence:** same-head GitHub Actions logs, local API-report/typecheck checks and focused test validation.

**Source:** [__tests__/native-protocol/AppleNativeProtocolV2.test.js:455](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/__tests__/native-protocol/AppleNativeProtocolV2.test.js#L455); [__tests__/ElectronIpcBoundary.test.js:541–545](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/__tests__/ElectronIpcBoundary.test.js#L541-L545); [native/tauri/src/btleplug_dispatcher.rs:17–24](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/btleplug_dispatcher.rs#L17-L24); [crates/ubm-mobile/tests/recording_peer_scope.rs:435](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-mobile/tests/recording_peer_scope.rs#L435).

The final commit fixes the seven stale API reports and now gets through packaging. In same-head CI run 37558288591, Linux Node 22 completes Jest with 7 failed suites / 8 failed tests / 5,279 passed tests (420 suites, 5,287 tests total). Failures include the two stale Electron timing assertions, the Apple 901-versus-900 file cap, two WinRT parameter TCK assertions, actual missing native parameter forwarding (R2-34), and two RN connection/bridge shape mismatches (R2-35). All four JS lanes are red. Tauri Linux fails rustfmt on the new import list. Windows Rust also fails the sustained-recording fixture because continuation setup step 0 reaches its command deadline. That timeout is an unresolved gate failure; this review has not established a distinct product root cause or proved it flaky. The packed Linux Bun gate now passes and Classic RN and Expo CNG Android builds pass; those improvements must be credited.

**Correction:** Fix actual forwarding and bridge-contract defects, complete TCK evidence for the newly supported parameter operation, correct the Apple file organization and Tauri formatting, and await explicit Electron retirement instead of six microtasks. Investigate the Windows recording timeout using its setup/deadline trace before changing budgets. Preserve all ownership assertions and run the unchanged canonical gates again.

**Acceptance:** At one commit, all required JS/native/format gates execute and pass. Keep packed Bun and Android successes. The local final-head API-report check passes; a standalone docs:check stops later only because this fresh worktree lacks generated build input, which is an environment prerequisite rather than a remaining API-report defect.

**Bundle evidence:** [evidence/final-ci-package-failures.log](evidence/final-ci-package-failures.log), [evidence/final-ci-112589503267.log](evidence/final-ci-112589503267.log), [evidence/final-ci-112589503295.log](evidence/final-ci-112589503295.log), [evidence/final-head-docs-check.log](evidence/final-head-docs-check.log), [evidence/final-head-typecheck.log](evidence/final-head-typecheck.log), [evidence/final-head-delta-jest.log](evidence/final-head-delta-jest.log), [evidence/ci-jobs.json](evidence/ci-jobs.json), [evidence/electron-retirement.md](evidence/electron-retirement.md).

### R2-02 — P1: An accepted external Pair can lose its link when UBM releases its last lease

**Origin:** new regression in the fix batch. **Scope:** Linux maintained BlueZ daemon. **Evidence:** complete production C functions executed with controlled OS boundaries.

**Source:** [vendor/bluez/ubm-le-gatt-5.87.patch:3287–3295](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L3287-L3295); [vendor/bluez/ubm-le-gatt-5.87.patch:767–771](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L767-L771); [vendor/bluez/ubm-le-gatt-5.87.patch:3813–3838](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L3813-L3838).

On an already-connected, unpaired LE device, another D-Bus sender starts Pair. The real Pair handler accepts the SMP security request and retains bonding state, but its admission stays uncommitted until bonding completes. protecting_interest counts in-flight read/write operations and committed admissions, so it ignores this accepted Pair. Releasing UBM’s final lease then submits a physical disconnect while the other app is still pairing, potentially while waiting for human input. The control verifies the disconnect request; it does not claim a measured HCI teardown.

**Correction:** Represent the lifetime of accepted asynchronous work explicitly and protect it until success, failure or cancellation. Preserve immediate rejection rollback and generation-aware arrival provenance. Counting only successful resource acquisition is too late for an active Pair.

**Acceptance:** Pause a foreign Pair after security-request acceptance, release the final lease, and assert no physical disconnect. Cover Pair success, refusal, cancellation, sender death and Pair opening the link before the first lease; retain rejected-Pair rollback controls. Then qualify the same pause with a real SMP agent/controller.

**Bundle evidence:** [evidence/linux.md](evidence/linux.md), [reproductions/linux-controls/README.md](reproductions/linux-controls/README.md), [reproductions/linux-controls/logs/current.log](reproductions/linux-controls/logs/current.log).

### R2-03 — P2: Protected logical release forgets the owner of deferred cleanup and token retirement

**Origin:** new lifecycle gap while correcting F09. **Scope:** Linux Rust client and BlueZ daemon. **Evidence:** production Rust path inspected; daemon lifecycle control executed.

**Source:** [crates/ubm-desktop/src/os/linux_lease.rs:381–397](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/os/linux_lease.rs#L381-L397); [vendor/bluez/ubm-le-gatt-5.87.patch:3298–3338](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L3298-L3338); [vendor/bluez/ubm-le-gatt-5.87.patch:3963–3965](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L3963-L3965).

The Rust client now correctly returns logical release for a protected receipt, but deletes the entry without terminal tracking or ACK maintenance. The daemon retains that nonterminal token, rejects its ACK and reconciles deferred physical cleanup only for dead owners. With a live sender and a temporary foreign read, finishing the read leaves the former UBM-created ACL without an active reconciliation owner while the sender stays alive. Even a later physical-loss observation leaves the live sender’s token allocated. Repeated overlapping successful sessions can exhaust the global 1,024-token limit. The new client test asserts the forgotten state without testing the real daemon’s opposing contract.

**Correction:** Complete logical retirement as one producer/consumer protocol: reclaim the token idempotently, preserve generation-scoped deferred physical cleanup and replay identity, and reconcile when the last temporary protector ends. Retained client maintenance may bridge the transition. Simply accepting protected ACKs and freeing the final owner is insufficient if it loses cleanup authority. Preserve the corrected public logical success.

**Acceptance:** Use real daemon/client integration for more than 1,024 overlapping cycles with two live senders, plus one live sender and a foreign read. Assert bounded retained tokens, cleanup after the final protector, no interference with the other lease, and correct duplicate/lost-reply/stale-generation behavior.

**Bundle evidence:** [evidence/linux.md](evidence/linux.md), [reproductions/linux-controls/logs/current.log](reproductions/linux-controls/logs/current.log).

### R2-04 — P2: Deferred AcquireWrite/AcquireNotify socket errors leave a durable interest

**Origin:** residual previous finding. **Scope:** Linux maintained BlueZ daemon. **Evidence:** complete deferred AcquireWrite and shared callback executed; corresponding AcquireNotify path source-inspected.

**Source:** [vendor/bluez/ubm-le-gatt-5.87.patch:1436–1447](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L1436-L1447).

When GATT is not ready, acquired-FD handlers commit an admission before waiting for characteristic_ready. If deferred create_sock later fails, for example socketpair returns EMFILE, the callback replies with an error and discards the request without abandoning that admission. The caller received no FD, yet its connection interest survives. The same immediate socket failure correctly rolls back. AcquireNotify shares the deferred socket path, so correcting only AcquireWrite’s immediate branch does not close this defect.

**Correction:** Carry the exact admission through deferred completion and abandon it on every failed delivery, along with any half-created subscription/socket resources. Separate accepted pending work from successful durable resource ownership.

**Acceptance:** Inject identical socket/resource errors in immediate and deferred write and notify acquisitions; assert one error, no FD, no retained admission and retryable cleanup for any half-created resource. Include cancellation and disconnect while readiness is pending.

**Bundle evidence:** [evidence/linux.md](evidence/linux.md), [reproductions/linux-controls/generated/function-provenance.json](reproductions/linux-controls/generated/function-provenance.json), [reproductions/linux-controls/logs/current.log](reproductions/linux-controls/logs/current.log).

### R2-05 — P2: Foreign one-shot GATT operations started before the first lease are unprotected

**Origin:** inherited gap found in the renewed review. **Scope:** Linux maintained BlueZ daemon. **Evidence:** complete production ReadValue and ownership guards executed.

**Source:** [vendor/bluez/ubm-le-gatt-5.87.patch:3669–3676](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L3669-L3676).

operation_begin returns immediately if no UBM lease peer exists. A foreign application can start an accepted native ReadValue on a daemon-connected link before the first ReserveLease. Both actual pre-dispatch and post-dispatch guards miss that read. UBM then adopts the connection and its final release requests physical disconnect while the foreign read is still active. No independent external Connect is needed in the reproduced scenario.

**Correction:** Track accepted one-shot activity by device and physical generation even before a lease peer exists. Adopt that observation when the first lease arrives, and retire it at exact completion/cancellation without converting ordinary reads into permanent holds.

**Acceptance:** Start a real accepted foreign read before ReserveLease, hold its completion, acquire/release UBM and assert no disconnect until the read ends. Cover failed admission, cancellation, peer removal and generation replacement.

**Bundle evidence:** [evidence/linux.md](evidence/linux.md), [reproductions/linux-controls/logs/current.log](reproductions/linux-controls/logs/current.log).

### R2-06 — P2: Physical loss before any lease exists leaves arrival records and stale ownership

**Origin:** new regression in the fix batch. **Scope:** Linux maintained BlueZ daemon. **Evidence:** production connection/loss handlers executed.

**Source:** [vendor/bluez/ubm-le-gatt-5.87.patch:4130–4145](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/bluez/ubm-le-gatt-5.87.patch#L4130-L4145).

The physical-loss handler returns when it cannot find a UBM peer, before cleaning global arrival and early-admission registries. Those registries now receive events before the first lease. In the control, 100 no-lease connection/loss cycles retain 100 stale arrival records. A completed external Connect admission also survives the end of its physical generation and can falsely classify a later daemon-created connection as externally owned.

**Correction:** Retire device/generation-scoped arrival and early-admission state on physical loss regardless of whether a lease peer was created. Audit device removal and attachment replacement as the same lifetime boundary.

**Acceptance:** Run repeated connect/loss cycles without any UBM lease and assert storage returns to baseline. Then end an early external generation and create a daemon-only generation; the old admission must not protect or relabel it.

**Bundle evidence:** [evidence/linux.md](evidence/linux.md), [reproductions/linux-controls/logs/current.log](reproductions/linux-controls/logs/current.log).

### R2-07 — P2: RN Apple writeWhenReady violates payload ownership, FIFO admission and invalidation

**Origin:** new regression in the F13 implementation. **Scope:** React Native Apple. **Evidence:** three reproductions through the actual public factory, provider and strict native binding.

**Source:** [src/backends/reactnative/react-native-rust-core-manager.ts:1521–1548](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-manager.ts#L1521-L1548); [src/backends/reactnative/react-native-rust-core-manager.ts:1493–1496](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-manager.ts#L1493-L1496); [docs/UNIFIED_SEMANTICS.md:949–962](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/docs/UNIFIED_SEMANTICS.md#L949-L962).

The new helper waits outside the connection operation queue and copies the caller’s bytes only when it eventually calls ordinary write. Reproductions show submitted [42] becomes native [99] after caller mutation; a later normal write [2] reaches native before the pending helper [1]; and a database-change event leaves the helper pending even though a subsequent read already rejects gatt.stale-handle. It settles only when an unrelated readiness edge or caller timeout arrives.

**Correction:** Use the same connection-owned admission coordinator as other writes, or an equivalent Rust-owner reservation implementing that contract. Validate/copy before the first await, wait at the FIFO head, preserve the original deadline and promptly settle on database invalidation, link end or destroy. An early copy alone fixes only one symptom.

**Acceptance:** Port the shared helper contract through the ordinary RN public factory: byte reuse, following same-link write, service change with no later readiness edge, abort/deadline before and after acquisition, disconnect and destroy. Assert native bytes/order/count and local/native resource retirement.

**Bundle evidence:** [reproductions/mobile-readiness-repro.test.ts](reproductions/mobile-readiness-repro.test.ts), [reproductions/mobile-bun-preload.ts](reproductions/mobile-bun-preload.ts), [evidence/mobile-readiness-repro.log](evidence/mobile-readiness-repro.log), [evidence/shared-mobile.md](evidence/shared-mobile.md), [evidence/final-head-mobile5.log](evidence/final-head-mobile5.log), [evidence/final-head-mobile-tauri.md](evidence/final-head-mobile-tauri.md).

### R2-08 — P2: RN readiness streams lose or hang on native source failure

**Origin:** new regression in the F13 implementation. **Scope:** React Native Apple. **Evidence:** two public stream fault-injection reproductions.

**Source:** [src/backends/reactnative/react-native-rust-core-provider.ts:1347–1370](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-provider.ts#L1347-L1370); [src/backends/reactnative/react-native-rust-core-provider.ts:2972–2979](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-provider.ts#L2972-L2979).

drainFailed closes the established scan/notification/adapter/general-event sources but omits the newly added readinessWatches. With successful later connection cleanup, a pending public readiness next() completes normally and loses the source error. If connection cleanup also fails, next() remains pending despite the irrecoverable source failure. Stream termination incorrectly depends on an independent cleanup operation succeeding.

**Correction:** Immediately close every readiness watch as source-failed with the normalized original error in the owner-wide drain-failure path. Keep native cleanup debt separately owned and retryable; it must not prevent a local terminal from being delivered.

**Acceptance:** Fail draining with a readiness next() pending, with successful, delayed and failed connection cleanup. Assert prompt public BleError with the original cause, zero retained local watches, preserved independent cleanup debt, and correct acquisition-time failure.

**Bundle evidence:** [reproductions/mobile-readiness-repro.test.ts](reproductions/mobile-readiness-repro.test.ts), [evidence/mobile-readiness-repro.log](evidence/mobile-readiness-repro.log), [evidence/shared-mobile.md](evidence/shared-mobile.md), [evidence/final-head-mobile5.log](evidence/final-head-mobile5.log), [evidence/final-head-mobile-tauri.md](evidence/final-head-mobile-tauri.md).

### R2-09 — P2: Electron/Tauri public characteristics still cannot writeWhenReady

**Origin:** residual previous finding. **Scope:** Electron renderer on macOS, Tauri on macOS. **Evidence:** actual shared public IPC adapter with a scripted host.

**Source:** [src/ipc/public-manager.ts:1173–1210](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/ipc/public-manager.ts#L1173-L1210); [src/public/gatt.ts:466–478](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/gatt.ts#L466-L478); [docs/UNIFIED_SEMANTICS.md:994–995](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/docs/UNIFIED_SEMANTICS.md#L994-L995).

The new IPC writeReadiness stream works, but createIpcGattSource never supplies writeWhenReady. The public characteristic therefore still throws capability.unsupported before any native write. The reproduction first reads readiness=true and then gets that refusal for the helper, with zero native writes. The updated semantics text says the helper follows the stream across these hosts, which is not implemented.

**Correction:** Connect the IPC public characteristic to the same owned/FIFO readiness-write operation used by the in-process implementation. Carry cancellation, original deadline, bytes and generation through the host boundary. Do not add a third ad hoc wait loop that repeats the RN defects.

**Acceptance:** Run the shared helper suite through the real Electron renderer and Tauri public factories on a readiness-capable host: ready/not-ready transitions, ordering, caller mutation, invalidation, cancellation and cleanup failure.

**Bundle evidence:** [reproductions/ipc-public-readiness-repro.ts](reproductions/ipc-public-readiness-repro.ts), [evidence/final-ipc-public-readiness-repro.log](evidence/final-ipc-public-readiness-repro.log).

### R2-11 — P2: Tauri watch quota is consumed permanently by successfully released handles

**Origin:** new regression. **Scope:** Tauri. **Evidence:** deterministic source accounting across subscribe, ACK, release, reset and caller teardown.

**Source:** [native/tauri/src/write_readiness.rs:289–298](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/write_readiness.rs#L289-L298); [native/tauri/src/write_readiness.rs:334–339](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/write_readiness.rs#L334-L339); [native/tauri/src/connection_parameters.rs:344–352](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/connection_parameters.rs#L344-L352); [native/tauri/src/connection_parameters.rs:393–398](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/connection_parameters.rs#L393-L398); [native/tauri/src/btleplug_dispatcher.rs:33](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/btleplug_dispatcher.rs#L33).

Each watch admission counts active handles plus released-handle tombstones against 256. A successful unsubscribe moves the fresh handle into a released set that normal event ACK, disconnect and adapter reset do not retire. Under one renderer lease, 256 sequential successful open/read/ACK/close cycles of one watch type exhaust its quota despite at most one live watch. The 257th acquisition returns stream.quota. Readiness and parameter quotas are separate. Replacing the entire renderer lease restores capacity; an ordinary stream close does not.

**Correction:** Separate the live-resource limit from bounded idempotent replay history, and define retirement using the stream/caller generation and acknowledgement protocol. Preserve duplicate-release safety without an ever-growing tombstone set or a lifetime-use quota disguised as a concurrency bound.

**Acceptance:** Perform more than 256 sequential successful cycles for each watch under the same renderer lease with events ACKed and zero live watches between cycles. Also test true concurrent overflow, duplicate release, late events, reset and caller replacement.

**Bundle evidence:** [evidence/tauri-watch-quota.md](evidence/tauri-watch-quota.md).

### R2-12 — P2: Scan accumulation still misses split service lists and native ingress drops needed evidence

**Origin:** partial previous fix. **Scope:** shared scan paths, WinRT native ingress. **Evidence:** actual shared evidence/matcher reproduction plus native filter inspection.

**Source:** [src/backend-contract/scan-evidence.ts:146–182](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L146-L182); [src/backend-contract/scan-evidence.ts:220–232](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L220-L232); [vendor/btleplug/src/winrtble/ble/watcher.rs:140–146](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/watcher.rs#L140-L146); [vendor/btleplug/src/winrtble/ble/watcher.rs:176–232](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/watcher.rs#L176-L232).

The common cache fixes a simple service-first/name-later pair, but replaces rather than unions two nonempty service UUID lists. services.all=[180D,180F] still fails when one UUID appears in each in-window packet. The WinRT prefilter also requires all pushed-down UUIDs in one event before admitting its address and drops a name-bearing scan response arriving before that packet. Evidence removed at ingress can never be reconstructed by the new common cache.

**Correction:** Define one bounded evidence-merging policy for partial lists and make native filtering conservative enough to retain every packet needed for residual matching. Avoid another independent native cache with different identity/expiry semantics. Preserve packet facts separately from accumulated query evidence.

**Acceptance:** Exercise two nonempty split lists, response-first/service-later, absent versus empty fields, multiple conditions and both packet orders through native ingress and public scan. Retain the simple pair positive control and test expiry and identity boundaries from R2-13/14.

**Bundle evidence:** [reproductions/scan-evidence-repro.ts](reproductions/scan-evidence-repro.ts), [evidence/scan-evidence-repro.log](evidence/scan-evidence-repro.log), [evidence/windows.md](evidence/windows.md), [evidence/final-head-scan-evidence-repro.log](evidence/final-head-scan-evidence-repro.log).

### R2-13 — P2: Unrelated scan packets refresh old field evidence beyond its expiry window

**Origin:** new cache defect. **Scope:** shared advertisement and IPC scan evidence. **Evidence:** actual production evidence and public query matcher executed.

**Source:** [src/backend-contract/scan-evidence.ts:88–111](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L88-L111).

The cache stores one timestamp for the whole merged record. Every newer packet refreshes the record while retaining omitted old fields. A name seen only at t=0 still matches at t=20 seconds under the declared 10-second window when unrelated packets arrive every five seconds. The IPC shape has the same behavior. A busy device can keep stale names/service facts eligible indefinitely.

**Correction:** Track freshness at the evidence granularity needed by each field/element. Expire observations by their original observation time; merging an already-derived record downstream must not freshen its constituent facts. Keep the time domain and generation explicit.

**Acceptance:** Keep unrelated traffic active beyond the window and verify old names, service members and manufacturer/service data stop matching. Check exact window boundaries, out-of-order packets and repeated downstream projection.

**Bundle evidence:** [reproductions/scan-evidence-repro.ts](reproductions/scan-evidence-repro.ts), [evidence/scan-evidence-repro.log](evidence/scan-evidence-repro.log), [evidence/final-head-scan-evidence-repro.log](evidence/final-head-scan-evidence-repro.log).

### R2-14 — P2: Scan evidence merges distinct public/random peer identities with identical address bits

**Origin:** new cache defect. **Scope:** shared address-bearing scan paths, WinRT ingress. **Evidence:** actual production cache and matcher reproduction.

**Source:** [src/backend-contract/scan-evidence.ts:114–121](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L114-L121); [vendor/btleplug/src/winrtble/ble/watcher.rs:176–204](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/watcher.rs#L176-L204).

The advertisement and IPC cache keys use the address bytes without the address type or distinct peer identity. Two peers with identical address bits but different public/random address types share evidence. The reproduction makes the random peer match a local-name filter using a name observed only from the public peer. WinRT’s numeric admitted-address set also lacks type separation.

**Correction:** Use the complete canonical peer identity within the owning adapter/scan generation, including address type where it is part of identity. Define conservative behavior for unknown type; do not merge records merely because one identity component is absent.

**Acceptance:** Use equal address bits with public versus random types and distinct peer IDs; also cover different adapters, unknown-to-known refinement and generation replacement. No query fact may cross those boundaries.

**Bundle evidence:** [reproductions/scan-evidence-repro.ts](reproductions/scan-evidence-repro.ts), [evidence/scan-evidence-repro.log](evidence/scan-evidence-repro.log), [evidence/final-head-scan-evidence-repro.log](evidence/final-head-scan-evidence-repro.log).

### R2-15 — P2: The scan evidence cache retains inactive peers without a capacity or expiry sweep

**Origin:** new cache defect. **Scope:** long-running shared scans, WinRT admitted addresses. **Evidence:** actual production map retention reproduction; native set inspected.

**Source:** [src/backend-contract/scan-evidence.ts:41–42](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L41-L42); [src/backend-contract/scan-evidence.ts:88–111](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backend-contract/scan-evidence.ts#L88-L111); [vendor/btleplug/src/winrtble/ble/watcher.rs:176–204](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/watcher.rs#L176-L204).

Expired entries are reconsidered only if that same identity sends again. There is no global eviction or capacity bound, and clear occurs at scan end. After 10,000 peers at t=0, a new observation at t=60 seconds still leaves 10,001 entries despite a 10-second evidence window. Continuous scans in an environment with many transient/randomized addresses retain all of them. WinRT’s admitted-address set is also unbounded for the scan lifetime.

**Correction:** Add bounded, generation-scoped retention with time-driven or admission-driven expiry and a defined overflow policy. Bound native and public intermediate state together; preserve explicit diagnostics/semantics when capacity affects evidence.

**Acceptance:** Run sustained transient-peer traffic across multiple windows, verify bounded memory/state without repeat packets from old peers, and ensure eviction never creates cross-peer matches or resurrects expired evidence.

**Bundle evidence:** [reproductions/scan-evidence-repro.ts](reproductions/scan-evidence-repro.ts), [evidence/scan-evidence-repro.log](evidence/scan-evidence-repro.log), [evidence/final-head-scan-evidence-repro.log](evidence/final-head-scan-evidence-repro.log).

### R2-16 — P2: A delayed initial parameter probe is emitted after a newer live event

**Origin:** new regression. **Scope:** Windows desktop provider, hosts using that provider. **Evidence:** actual production watch and event methods with controlled native scheduling.

**Source:** [src/backends/desktop/desktop-rust-core-provider.ts:2908–2927](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L2908-L2927); [src/backends/desktop/desktop-rust-core-provider.ts:2946–2990](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L2946-L2990).

The watch is registered before awaiting its initial native probe. Live events immediately publish while the probe is pending; the delayed probe then publishes unconditionally. A 30,000-microsecond probe followed by a newer 60,000-microsecond event yields [60000,30000], with increasing ordinals and emission times. The stale sample is presented as the newest measurement. The real NAPI consumer path is currently blocked earlier by R2-34; the source-level defect remains independently demonstrated and must also be fixed when that forwarding path is restored. No live WinRT execution is claimed.

**Correction:** Order initial sampling and live updates explicitly: buffer/replay events around initial publication or discard an obsolete probe. Preserve observation authority and timestamps. The adjacent readiness implementation already has initial-probe buffering machinery to compare.

**Acceptance:** Test event-before-probe-resolution, multiple buffered events, probe failure and disconnect during acquisition. The final public sample must be the newest valid observation, never an old sample with a fresh authority marker.

**Bundle evidence:** [reproductions/parameter-watch-race.ts](reproductions/parameter-watch-race.ts), [evidence/parameter-watch-race.log](evidence/parameter-watch-race.log), [evidence/windows.md](evidence/windows.md), [evidence/final-parameter-watch-race.log](evidence/final-parameter-watch-race.log), [evidence/final-head-windows.md](evidence/final-head-windows.md).

### R2-17 — P2: Parameter events accept invalid measurements that the snapshot API rejects

**Origin:** new regression. **Scope:** public parameter streams, Windows native parameter source. **Evidence:** actual public factory reproduction and Microsoft API contract.

**Source:** [src/public/ble-manager.ts:1283–1293](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1283-L1293); [src/public/ble-manager.ts:1189–1203](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1189-L1203); [src/public/ble-manager.ts:1784–1796](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1784-L1796); [vendor/btleplug/src/winrtble/ble/device.rs:78–96](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/device.rs#L78-L96).

The stream mapper validates connection identity but not interval, timeout or latency. The same all-zero observation is rejected by controls.parameters() as protocol.violation and emitted by parameterEvents() as state:measured with zero interval/timeout. Microsoft documents an all-zero result when GetConnectionParameters observes a disconnected device, so this is also a plausible native disconnect race, not solely malformed test input. The real NAPI consumer path is currently blocked earlier by R2-34; the source-level defect remains independently demonstrated and must also be fixed when that forwarding path is restored. No live WinRT execution is claimed.

**Correction:** Use one validator for snapshot and stream observations. Classify native disconnected/all-zero results as loss/unavailability instead of a measured link, and preserve typed source failure. Do not fabricate defaults when the OS has no valid measurement.

**Acceptance:** Test zero, NaN, nonfinite/negative values, fractional/negative latency, valid unit conversion and a disconnect crossing the native read. Both public APIs must agree.

**Bundle evidence:** [reproductions/parameter-invalid-stream.ts](reproductions/parameter-invalid-stream.ts), [evidence/parameter-invalid-stream.log](evidence/parameter-invalid-stream.log), [evidence/windows.md](evidence/windows.md), [evidence/final-parameter-invalid-stream.log](evidence/final-parameter-invalid-stream.log), [evidence/final-head-windows.md](evidence/final-head-windows.md).

### R2-18 — P2: Lost-link parameter reads use a different error vocabulary from other link controls

**Origin:** new regression. **Scope:** Windows connection parameter reads. **Evidence:** actual pinned Rust crate built and its error classifier executed.

**Source:** [crates/ubm-desktop/src/errors.rs:276–282](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/errors.rs#L276-L282); [crates/ubm-desktop/src/central_parity.rs:440–448](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/central_parity.rs#L440-L448); [crates/ubm-desktop/src/btleplug_backend.rs:4539–4562](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/btleplug_backend.rs#L4539-L4562).

The established-link operation allowlist omits connection.parameters. Identical btleplug/not-connected evidence becomes connection.lost for RSSI and effective MTU, but platform.failure for the new parameters operation. The reproduction builds the pinned crate and executes that classifier. Requested release consequently also misses the ordinary operation.disconnected renaming path. This finding concerns error semantics and reconnect-policy input; it does not alone prove an ownership mutation. The real NAPI consumer path is currently blocked earlier by R2-34; the source-level defect remains independently demonstrated and must also be fixed when that forwarding path is restored. No live WinRT execution is claimed.

**Correction:** Classify the new operation as an established-link operation. Prefer an exhaustive typed operation classification over an indefinitely growing string allowlist, and retain native detail.

**Acceptance:** Extend the existing link-loss/security matrix with parameter read and requested release. The same observed link end must produce the same public code across equivalent controls and hosts.

**Bundle evidence:** [reproductions/parameter-link-loss.rs](reproductions/parameter-link-loss.rs), [reproductions/run-parameter-link-loss.py](reproductions/run-parameter-link-loss.py), [evidence/parameter-link-loss.log](evidence/parameter-link-loss.log), [evidence/parameter-link-loss-build.log](evidence/parameter-link-loss-build.log).

### R2-19 — P2: Desktop code drops a soft delivery preference that WinRT can honor

**Origin:** remaining gap exposed by the hard-requirement fix. **Scope:** WinRT; compare Android. **Evidence:** production dispatch/selection paths and current regression test inspected.

**Source:** [src/backends/desktop/desktop-rust-core-provider.ts:4524–4527](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L4524-L4527); [src/backends/desktop/desktop-rust-core-provider.ts:4549–4559](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L4549-L4559); [crates/ubm-desktop/src/delivery.rs:76–80](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/delivery.rs#L76-L80); [crates/ubm-mobile/src/session.rs:2400–2409](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-mobile/src/session.rs#L2400-L2409).

Both hard require modes now reach the core, closing the original F03. Every prefer mode is still discarded. On a characteristic supporting both modes, prefer-indication therefore reaches WinRT without a preference and defaults to Notification, although this revision can select the first CCCD mode. Android preserves the soft preference separately. A new desktop test explicitly expects preference loss on every host.

**Correction:** Carry required and preferred mode as distinct values through desktop/core/NAPI/IPC. Honor the supported preference on WinRT before the first CCCD write, retain truthful fallback where an OS cannot choose, and report actual delivery. Do not turn a soft preference into a hard requirement.

**Acceptance:** Cover dual-property prefer-indication, unsupported preferred-property fallback, both require modes and existing shared physical subscriptions on Windows and Android, with explicit OS fallback behavior on Apple/BlueZ.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md), [evidence/desktop-hard-delivery.log](evidence/desktop-hard-delivery.log).

### R2-20 — P2: Initial WinRT discovery and compound subscription rollback still flatten native errors

**Origin:** residual previous finding. **Scope:** WinRT. **Evidence:** current production failure branches inspected.

**Source:** [vendor/btleplug/src/winrtble/peripheral.rs:449–462](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/peripheral.rs#L449-L462); [vendor/btleplug/src/winrtble/ble/device.rs:226–243](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/device.rs#L226-L243); [vendor/btleplug/src/winrtble/utils.rs:27–37](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/utils.rs#L27-L37).

Ordinary new read/write/discovery/CCCD errors now preserve typed status/ATT/HRESULT detail. Two reachable paths still do not: a typed CCCD failure plus RemoveValueChanged failure becomes one Error::Other string; initial connection GetGattServices keeps only status and invokes the old mapper, which loses the ATT byte and treats ProtocolError as NotSupported. This differs from the corrected rediscovery path and can destroy useful security/link classification and cleanup diagnostics.

**Correction:** Use the same result-bearing structured projection for initial discovery and preserve primary plus cleanup failures as separate structured records. Keep ownership of a handler whose removal failed so cleanup remains retryable.

**Acceptance:** Inject CCCD ProtocolError with ATT authentication plus a rollback HRESULT failure, and initial discovery ProtocolError/ATT, AccessDenied without ATT and Unreachable. Verify public code/detail and retained cleanup obligation.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md).

### R2-22 — P2: Native GATT graphs fabricate primary services and empty inclusion relationships

**Origin:** inherited completeness gap. **Scope:** desktop native, React Native, Tauri. **Evidence:** native models, wire models and public projection traced.

**Source:** [crates/ubm-desktop/src/boundary.rs:818–824](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/boundary.rs#L818-L824); [src/backends/desktop/desktop-rust-core-provider.ts:4184–4185](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L4184-L4185); [src/backends/reactnative/react-native-rust-core-provider.ts:3190](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-provider.ts#L3190); [native/tauri/src/btleplug_dispatcher.rs:6014–6023](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/native/tauri/src/btleplug_dispatcher.rs#L6014-L6023); [src/public/gatt.ts:368–389](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/gatt.ts#L368-L389).

The shared native service snapshot carries neither primary/secondary status nor included-service relationships. Desktop, RN and Tauri fill primary:true and includedServices:[] anyway, so unknown or discarded facts become confident public assertions. Tauri explicitly leaves primary metadata to a follow-up. The vendored CoreBluetooth layer already reads CBService.isPrimary, but the desktop mapper drops it; mobile Swift/Android and strict wire models likewise omit these fields. Secondary or included services cannot be represented faithfully. The last commit fixes Tauri restriction metadata but moves these same primary/includes defaults into ipc_service_record; it does not correct the graph facts.

**Correction:** Propagate observed primary status and inclusion relationships through one shared graph and all native/IPC codecs. Discover them where supported. Where a platform does not provide a fact, represent unknown/unavailable explicitly rather than synthesizing true or an empty set. Preserve service occurrence identity when resolving inclusion edges.

**Acceptance:** Use a database with primary and secondary services, included-service relationships and duplicate UUID occurrences. Compare public graphs across native, RN, Electron and Tauri; test incomplete discovery and stale generations without fabricated metadata.

**Bundle evidence:** [evidence/shared-mobile.md](evidence/shared-mobile.md).

### R2-23 — P2: Windows preferred connection presets remain unreachable from requestPriority

**Origin:** residual previous finding. **Scope:** Windows 11 build 22000+. **Evidence:** public/native route inventory and primary Microsoft API.

**Source:** [crates/ubm-desktop/src/capabilities.rs:408–415](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L408-L415); [src/core/core-connection-controls.ts:145–147](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/core/core-connection-controls.ts#L145-L147); [src/public/ble-manager.ts:1682–1713](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1682-L1713); [vendor/btleplug/src/winrtble/ble/device.rs:337–374](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/device.rs#L337-L374).

The new parameter read/watch route does not implement the request/control half of F17. connection:priority still has no OS override and the backend hook is absent. The vendored WinRT preset wrapper already exists but is unused. Balanced, ThroughputOptimized and PowerOptimized correspond to the public balanced/high-throughput/low-power requests on API-capable Windows.

**Correction:** Complete the owned public→provider→core→WinRT request path and host projections, with runtime API detection, typed statuses and explicit request-object lifetime. Keep request acceptance separate from observed parameter values; successful intent is not a measurement.

**Acceptance:** Test each preset, rejection status, pre-22000 runtime refusal, disconnected request, cancellation/deadline and request lifetime. No successful request may fabricate interval/latency/timeout observations.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md).

### R2-24 — P2: Parameter capability registration ignores the actual Windows API floor

**Origin:** new implementation remains statically advertised. **Scope:** Windows before build 22000. **Evidence:** capability registration and native runtime guard inspected.

**Source:** [crates/ubm-desktop/src/capabilities.rs:417–428](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L417-L428); [crates/ubm-desktop/src/capabilities.rs:681–693](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L681-L693); [crates/ubm-desktop/src/capabilities.rs:732–779](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L732-L779); [vendor/btleplug/src/winrtble/ble/device.rs:58–96](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/device.rs#L58-L96).

Every compiled Windows target registers connection:parameters as provided/limited. Method presence is checked only when the native operation runs. On an older Windows host, supports can therefore report usable support even though every call is known to be unavailable. A Windows-11 floor in limitation prose does not satisfy the repository’s instantiated-runtime capability requirement. The call guard avoids blindly calling an absent API; this is not a crash claim.

**Correction:** Carry runtime API presence into the instantiated capability set and expose the precise unavailable/unsupported reason on hosts below the floor. Keep limited only for actual available functionality with real restrictions.

**Acceptance:** For the same compiled target, test runtime profiles with and without the API and verify snapshot/watch capability projections through direct and IPC hosts.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md).

### R2-25 — P2: Native parameter-event failures and early queue loss remain invisible to consumers

**Origin:** new regression. **Scope:** WinRT parameter event source. **Evidence:** production callback and relay failure branches inspected.

**Source:** [vendor/btleplug/src/winrtble/ble/device.rs:164–183](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/device.rs#L164-L183); [crates/ubm-desktop/src/btleplug_backend.rs:2089–2113](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/btleplug_backend.rs#L2089-L2113).

The WinRT event callback ignores a failed read_connection_parameters via if let Ok and returns success, which can leave the last sample stale without a source error. The vendor-to-radio relay turns Lagged into only a diagnostic counter and silently stops on Closed. Lag can lose intermediate transitions without a continuity indication even when a newer queued value subsequently arrives. Downstream NAPI lag reconciliation is not triggered by this earlier source loss.

**Correction:** Transport a typed failed/uncertain or reconciliation event from the originating boundary. Make queue lag produce observable discontinuity or guaranteed fresh reconciliation, and terminate closed sources explicitly. Retain the original source cause and independent cleanup ownership.

**Acceptance:** Inject getter failure during an OS callback, overflow the vendor-to-radio queue independently of the NAPI queue, and close the source. Assert a public terminal or successful fresh reconciliation with explicit continuity semantics.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md).

### R2-26 — P2: Peer directories still omit OS-supported known or system-connected retrieval

**Origin:** inherited completion gaps. **Scope:** Linux, Windows, Android, iOS. **Evidence:** current public/native route inventory plus primary OS APIs.

**Source:** [crates/ubm-desktop/src/capabilities.rs:270–289](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L270-L289); [src/backends/desktop/desktop-peer-directory.ts:91–139](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-peer-directory.ts#L91-L139); [crates/ubm-desktop/src/btleplug_backend.rs:3458–3558](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/btleplug_backend.rs#L3458-L3558); [src/backends/reactnative/react-native-rust-core-provider.ts:1816–1820](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-provider.ts#L1816-L1820); [crates/ubm-mobile/src/session.rs:1684–1706](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-mobile/src/session.rs#L1684-L1706).

Linux and Windows known()/system-connected native routes remain unsupported; reference resolution is restricted to bonded inventory, missing known unbonded OS records. The code already performs owner-fenced BlueZ object enumeration and WinRT paired-device enumeration. Mobile connected() reads only cached/restored/core-owned records, and rejects all nonempty service filters, so a fresh manager misses a peer connected by another app; the service-scoped iOS case refuses outright. BlueZ object/bearer state, WinRT connected selectors, Android BluetoothManager.getConnectedDevices(GATT), and CoreBluetooth service-scoped retrieval provide supported subsets.

**Correction:** Implement genuinely distinct known, bonded and system-connected queries through existing owned native boundaries. Make Apple’s references/services prerequisites platform-specific. Preserve adapter identity, owner fencing, actual OS visibility and native failures; inventory membership must never create a local connection lease or disconnect authority. For dual-mode BlueZ, Device1.Connected alone is not proof of an LE connection: use positive bearer-specific facts where available. Do not promise unrestricted historical enumeration or Android service facts the inventory does not supply.

**Acceptance:** Exercise explicit paired/cached/unbonded/unavailable references, a foreign-app-connected peer absent from local caches, service criteria on Apple, adapter filtering, BR/EDR-only BlueZ state, native disappearance and owner replacement. Assert no scan/connect/pair/lease side effects. Cover direct and IPC/mobile public calls.

**Bundle evidence:** [evidence/linux-completeness.md](evidence/linux-completeness.md), [evidence/windows-omissions.md](evidence/windows-omissions.md), [evidence/shared-mobile.md](evidence/shared-mobile.md).

### R2-27 — P2: Optional BlueZ acquired-FD GATT transports are explicitly left unimplemented

**Origin:** inherited implementation gap. **Scope:** Linux Node/Bun Rust route. **Evidence:** current first-party TCK/feature inventory and verified BlueZ source.

**Source:** [src/tck/first-party/desktop-rust-core-tck-registration.ts:322–331](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/tck/first-party/desktop-rust-core-tck-registration.ts#L322-L331); [crates/ubm-desktop/src/capabilities.rs:634–639](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L634-L639); [src/backends/desktop/desktop-rust-core-provider.ts:5130–5194](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L5130-L5194).

The current first-party Rust registration excludes bluez:acquire-write and bluez:acquire-notify explicitly because they are not implemented. A bounded-sequential-only capability limitation does not implement the FD transports. The maintained BlueZ producer already offers AcquireWrite/AcquireNotify on eligible characteristics, returning an FD and actual MTU. This is a supported-feature completion gap, not a claim that normal WriteValue/StartNotify are broken.

**Correction:** Provide the owned FD acquisition/transport route with runtime method/flag availability, cancellation, backpressure, HUP/error handling and teardown. Preserve real acquisition conflicts and native errors. Do not label sequential writes as an acquired transport or use missing radio qualification as a reason to leave the software route absent.

**Acceptance:** Test write and notify acquisitions, optional-method absence, flag restrictions, conflicts, cancellation before/after FD delivery, close/HUP, reconnection/MTU change, bounded buffering and daemon-interest cleanup through actual Node and Bun consumers.

**Bundle evidence:** [evidence/linux-completeness.md](evidence/linux-completeness.md).

### R2-28 — P2: Android subrate requests remain an unconditional stub on capable authorized hosts

**Origin:** inherited implementation gap. **Scope:** Android SDK 36.1+. **Evidence:** current public/native inventory and primary Android API.

**Source:** [src/public/ble-manager.ts:1760–1763](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1760-L1763); [src/public/ble-manager.ts:1816–1817](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L1816-L1817); [src/backends/reactnative/react-native-rust-core-features.ts:327–395](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-features.ts#L327-L395).

requestSubrate always throws unsupported and no mobile radio request/callback route exists. Android’s public requestSubrateMode and onSubrateChange exist from 36.1. An ordinary companion-associated application with BLUETOOTH_CONNECT can invoke the request; BLUETOOTH_PRIVILEGED is an alternative, not a universal prerequisite. A connected/bonded capable peer with those conditions still cannot use the existing public UBM control.

**Correction:** Implement the owned command and callback path, public registration, typed native results and full SDK-version admission. 36.0 and 36.1 must not be collapsed into the same major-version check. Preserve permission/association/controller refusal and cancellation/generation semantics. Requested mode must not become a fabricated measured subrate factor or connection-parameter observation.

**Acceptance:** Invoke the real public path on native fixtures for 36.0 versus 36.1, association/permission/native failures, acceptance versus callback result, deadlines and stale completion. Current TCK coverage that records invoked:false cannot establish implementation.

**Bundle evidence:** [evidence/shared-mobile.md](evidence/shared-mobile.md).

### R2-29 — P2: Android security discards encryption observations that public APIs provide

**Origin:** inherited implementation gap. **Scope:** Android API 36 events; 36.1 snapshot. **Evidence:** current native source and primary Android API.

**Source:** [android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedRadioPort.kt:324–337](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedRadioPort.kt#L324-L337); [android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt:1078–1124](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt#L1078-L1124).

The native security projection hardcodes encryption=unsupported with a no-public-API explanation and the receiver listens only to bond-state changes. Android exposes ACTION_ENCRYPTION_CHANGE from API 36 and getEncryptionStatus(TRANSPORT_LE) from 36.1, using ordinary CONNECT permission. Security state/watch therefore discard an available link observation even on the current target-36 platform.

**Correction:** Carry transport- and generation-correlated encryption observations into the shared security snapshot/watch with precise runtime floors and stale-state retirement. The 36.1 query can return null for unencrypted or disconnected, so it is not standalone proof of an unencrypted live link. Keep authentication and Secure Connections unknown unless independently established.

**Acceptance:** Test API 35 fallback, API 36 encryption changes without bond transitions, 36.1 snapshots, BR/EDR versus LE separation, stale callbacks, connection loss and receiver cleanup/retry.

**Bundle evidence:** [evidence/shared-mobile.md](evidence/shared-mobile.md).

### R2-30 — P2: Android rejects public batching and PHY scan options instead of implementing them

**Origin:** inherited implementation gap. **Scope:** Android scan. **Evidence:** public options, production native scan and primary Android API.

**Source:** [src/public/ble-manager.ts:392–402](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L392-L402); [src/backends/reactnative/react-native-rust-core-provider.ts:1953–1967](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-provider.ts#L1953-L1967); [android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt:823–858](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt#L823-L858); [android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt:881–904](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt#L881-L904).

The public type exposes reportDelayMs and phy, but the RN provider rejects either unconditionally, including reportDelayMs:0. Native code applies neither ScanSettings option and lacks onBatchScanResults. setReportDelay exists from API 21, below the package minimum; setPhy exists from API 26 for nonlegacy scanning with actual adapter support. Removing only the TS rejection would still leave the feature incomplete.

**Correction:** Complete typed options through the native wire/settings builder and implement owned bounded batch delivery. Gate PHY choices against actual OS/adapter support, preserve native refusal and acknowledge OS batching timing constraints.

**Acceptance:** Call the ordinary public scan with zero delay, positive batching and nonlegacy 1M/coded/all-supported PHY. Check actual native settings, batches, byte ownership, quotas, stop/cancellation and unsupported combinations.

**Bundle evidence:** [evidence/shared-mobile.md](evidence/shared-mobile.md).

### R2-31 — P2: Windows scan controls are a placeholder despite supported WinRT options

**Origin:** inherited implementation gap. **Scope:** Windows scan. **Evidence:** current public/native inventory and primary WinRT API.

**Source:** [src/public/ble-manager.ts:403–410](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/public/ble-manager.ts#L403-L410); [src/backends/desktop/desktop-rust-core-provider.ts:2451–2453](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L2451-L2453); [vendor/btleplug/src/winrtble/ble/watcher.rs:104–123](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/ble/watcher.rs#L104-L123); [crates/ubm-desktop/src/capabilities.rs:237–243](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/capabilities.rs#L237-L243).

The WinRT platform-options type accepts only kind:winrt, additional controls are invalid, and the provider refuses any platform options. Native scanning is fixed to Active and extended advertisements remain disabled. WinRT offers Passive/Active scanning and, on build 19041+, None reception and AllowExtendedAdvertisements. The current blanket refusal is a library omission for those supported subsets.

**Correction:** Expose concrete typed Windows scan controls, pass them through planning/ownership/native configuration and detect actual API/adapter support. Retain conservative defaults if desired. Do not imply Windows supports Android batching/PHY controls that its API does not expose.

**Acceptance:** Verify active/passive/none mapping, extended-advertisement opt-in and payload mapping, precise unsupported runtime profiles, adapter refusal and unchanged shared cancellation/cleanup semantics.

**Bundle evidence:** [evidence/windows-omissions.md](evidence/windows-omissions.md).

### R2-32 — P2: Passing Linux Bun gates still omit the real packed desktop integration and two operating systems

**Origin:** partially improved previous qualification gap. **Scope:** Bun Linux, Bun macOS, Bun Windows. **Evidence:** current CI step results and qualifier source inspection.

**Source:** [.github/workflows/ci.yml:159–167](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/.github/workflows/ci.yml#L159-L167); [.github/workflows/ci.yml:319–348](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/.github/workflows/ci.yml#L319-L348); [scripts/ci/bun-packed-desktop-acceptance.js:120–168](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/scripts/ci/bun-packed-desktop-acceptance.js#L120-L168); [scripts/ci/bun-packed-desktop-acceptance.js:182–214](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/scripts/ci/bun-packed-desktop-acceptance.js#L182-L214).

At the final head, both the Linux source-mode Bun 1.4.2 smoke and packed Bun desktop acceptance pass in CI. The packed qualifier verifies exact installed native identity and an actual raw-native event waker. However, imports of desktop factories are only checked for typeof function; its six public scenarios use createDeterministicManagerScenarioFactory, while a separate native scenario opens a raw synthetic central. Neither path joins the public desktop factory/provider to native. Both Bun gates are Linux-only: macOS and Windows have no same-head Bun packed/public-route CI qualification in this evidence. This does not erase earlier low-level/manual receipts. R2-34 is a concrete example of the missing integration: a production NAPI forwarding omission survives the passing packed tests. No new Bun-specific loader incompatibility was established.

**Correction:** Keep the useful low-level and generic tests, then run common scenarios through the actual packed desktop factory/provider with the synthetic native boundary on all three OSes, under Node and Bun. Exercise actual CJS and ESM operations, native events, cancellation/deadline, stale completion and cleanup. Maintain separate change-scoped physical-radio evidence.

**Acceptance:** A successful same-head three-OS matrix must instantiate the real consumer route and assert events/waker delivery and zero-resource retirement. Source smoke, packed integration and physical evidence should each state their own scope. A Linux import of another OS factory is not execution on that OS.

**Bundle evidence:** [evidence/bun-qualification.md](evidence/bun-qualification.md), [evidence/ci-jobs.json](evidence/ci-jobs.json), [evidence/final-head-windows.md](evidence/final-head-windows.md).

### R2-33 — P3: Windows scan planning still says connectability is unavailable after the native fix

**Origin:** residual planning metadata. **Scope:** Windows scan planner. **Evidence:** native observation mapping and planner input inspected.

**Source:** [vendor/btleplug/src/winrtble/gatt_model.rs:56–74](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/gatt_model.rs#L56-L74); [vendor/btleplug/src/winrtble/peripheral.rs:237–242](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/vendor/btleplug/src/winrtble/peripheral.rs#L237-L242); [src/backends/desktop/desktop-rust-core-provider.ts:258–269](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/desktop/desktop-rust-core-provider.ts#L258-L269).

Native connectability is now mapped from actual legacy event type or the extended flag, closing the original data defect. The provider’s observationFields still omits connectable, so planning/explanation metadata reports it unavailable. The residual public matcher can use the delivered field; this is not a claim that scan execution necessarily rejects the query.

**Correction:** Make the WinRT planning profile reflect the observations that this backend actually supplies, with unknown retained for packets that do not establish connectability. Keep profile capability and per-packet evidence distinct.

**Acceptance:** Build a connectability query plan using the real profile and compare its explanation with legacy, scan-response and extended packet observations. No false unavailable annotation and no invented false for unknown scan responses.

**Bundle evidence:** [evidence/windows.md](evidence/windows.md).

### R2-34 — P2: The NAPI dispatch wrapper never forwards connection_parameters

**Origin:** introduced with the parameter implementation; exposed once the package gate opened. **Scope:** Windows Node, Windows Bun, Electron on Windows. **Evidence:** actual production call chain and independently failing focused forwarding guard.

**Source:** [bindings/napi/src/dispatch.rs:221–247](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/bindings/napi/src/dispatch.rs#L221-L247); [bindings/napi/src/dispatch.rs:2912](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/bindings/napi/src/dispatch.rs#L2912); [bindings/napi/src/dispatch.rs:3569–3583](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/bindings/napi/src/dispatch.rs#L3569-L3583); [crates/ubm-desktop/src/boundary.rs:1292–1304](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/boundary.rs#L1292-L1304); [crates/ubm-desktop/src/central_parity.rs:436](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/crates/ubm-desktop/src/central_parity.rs#L436).

UbmCentral wraps the actual radio in DispatchRadio before constructing DesktopCentral. The wrapper implements RadioBoundary but omits connection_parameters, so the exported NAPI call executes the trait default that returns capability.unsupported. The real WinRT getter is never reached. Both the public snapshot and parameter-watch initial acquisition need this probe, so the advertised operation fails through Node/Bun/Electron even on a connected API-capable Windows host. This is an implementation omission, not a stale source-test expectation. Tauri opens its native central directly and is outside this specific wrapper defect.

**Correction:** Forward the method through both Radio and Synthetic variants, preserving its parameters, error and operation lifetime. Complete the synthetic fixture’s parameter observation support and prove the actual packed consumer route. Update required parameter TCK evidence to exercise a measured observation instead of merely registering the capability.

**Acceptance:** The focused forwarding guard currently reports 1 failed / 49 passed. Make it pass, then call parameters() and acquire parameterEvents() through the real NAPI/provider/public route with a controlled native radio. Assert native invocation and value/error propagation. Retain separate tests for R2-16–18/24–25, which are not fixed by forwarding alone.

**Bundle evidence:** [evidence/napi-parameter-forwarding.md](evidence/napi-parameter-forwarding.md), [evidence/final-dispatch-forwarding-jest.log](evidence/final-dispatch-forwarding-jest.log), [evidence/final-ci-package-failures.log](evidence/final-ci-package-failures.log).

### R2-35 — P2: The exported RN internal connection and bridge shape omit the new parameter methods

**Origin:** introduced when the shared Connection contract gained parameter controls. **Scope:** React Native internal/advanced manager. **Evidence:** actual exported Android and Apple environment-factory handles reproduced; production shape table and current CI.

**Source:** [src/manager/ble-manager.ts:728–733](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/manager/ble-manager.ts#L728-L733); [src/backends/reactnative/react-native-rust-core-manager.ts:250–265](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-manager.ts#L250-L265); [src/backends/reactnative/react-native-rust-core-manager.ts:325–330](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-manager.ts#L325-L330); [src/backends/reactnative/react-native-rust-core-manager.ts:1029–1038](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/backends/reactnative/react-native-rust-core-manager.ts#L1029-L1038); [src/react-native.ts:30](https://github.com/sfourdrinier/unified-ble-manager/blob/b80542f39144949738b04dc7d19b4a7ee52cc8c6/src/react-native.ts#L30).

The shared Connection now includes parameters() and parameterEvents(), but RN NativeConnection implements neither and BRIDGE_SHAPES.connection omits both. The production shape assertion consequently accepts an incomplete handle. The exported createReactNativeBleManagerWithEnvironment returns this typed internal connection; invoking its promised methods can throw TypeError. Current CI detects both missing methods in the native-manager parity and bridge-shape tests. Ordinary public controls remain guarded by capability/method checks, so this finding does not mean all public mobile connections or parameter calls crash.

**Correction:** Implement the normalized, capability-gated methods on NativeConnection and add them to the authoritative bridge table so validation covers them. On a platform without parameter observations, the methods should provide the shared unsupported behavior rather than be absent. Do not remove the common interface or weaken parity assertions to hide the gap.

**Acceptance:** Assert actual exported RN internal handles expose every shared connection method; invoke both absent-capability methods and receive the correct normalized refusal rather than TypeError. Keep the ordinary public capability behavior unchanged and run both production shape tests.

**Bundle evidence:** [evidence/final-head-mobile-tauri.md](evidence/final-head-mobile-tauri.md), [evidence/final-head-ci-triage.md](evidence/final-head-ci-triage.md), [reproductions/native-connection-shape-repro.test.ts](reproductions/native-connection-shape-repro.test.ts), [evidence/native-connection-shape-repro.log](evidence/native-connection-shape-repro.log), [evidence/final-ci-package-failures.log](evidence/final-ci-package-failures.log).

## Previous review closure

“Closed” below means the original identified defect has a source fix and the stated evidence. It does not mean every native behavior was physically tested or that nearby newly identified defects are excused.

| Previous ID | Disposition | Evidence / remaining work |
|---|---|---|
| F01 | Closed in source | Real uncached WinRT descriptor enumeration and indexing replace empty success. Native radio not run here. |
| F02 | Original scenario closed | An earlier committed external Connect now protects a borrowed lease. R2-02 and R2-05 are different still-unprotected operations. |
| F03 | Original hard-requirement defect closed | Both hard modes are forwarded, and subscription keys distinguish requirements. Soft preference remains R2-19. |
| F04 | Closed in source at final head | The last commit adds the missing Tauri service-row restriction projection; R2-21 is closed. Primary/includes metadata is a separate surviving finding, R2-22. |
| F05 | Partial | Simple name/service split works. Split nonempty lists, ingress filtering and new cache faults remain: R2-12–15. |
| F06 | Closed | FIFO dirty-scope scheduling fixes starvation beyond one 32-scope batch; full mobile Rust suite passed. |
| F07 | Partial | Immediate rejected admissions roll back. Deferred acquired-socket failures still leak: R2-04. |
| F08 | Original scenario closed | Dead-owner cleanup resumes after the foreign read. R2-03 concerns the different newly accepted live-sender retirement. |
| F09 | Public result corrected; lifetime partial | Protected non-final release is logical success, but token/deferred cleanup ownership is lost: R2-03. |
| F10 | Closed in source | WinRT selects the initial CCCD mode before enabling; the old Indicate-then-Notify sequence is removed. |
| F11 | Partial | Normal structured errors improved. Initial connect and compound rollback still flatten native detail: R2-20. |
| F12 | Closed | Apple when-available uses real pending connect and caller deadline/cancellation; Rust and relevant public-boundary tests passed. |
| F13 | Mechanism implemented; behavior incomplete | Apple RN readiness plumbing exists. New helper admission and source-failure defects remain: R2-07–08. |
| F14 | Partial | IPC readiness/parameter streams exist and their typed error projection is now fixed. The characteristic helper remains unwired: R2-09; cumulative Tauri quota remains R2-11. |
| F15 | Closed in source and deterministic checks | Apple ATT MTU remains unobserved while true per-mode write capacities stay available; no conversion of capacity to a measured ATT MTU. |
| F16 | Native data closed; planner partial | Legacy/extended connectability now observed truthfully; planner field inventory remains stale: R2-33. |
| F17 | Partial | Low-level WinRT getter/events exist, but the real NAPI dispatch wrapper omits forwarding (R2-34), presets remain absent, and parameter semantics still have R2-16–18/23–25. |
| R1 | Addressed at production control-flow level | Successful explicit LE Pair clears suppression and permits Medium-to-High retry on the same attachment. Actual SMP/controller qualification remains required. |
| Q1 | Partial | Both Linux source-mode and packed Bun gates pass at the final head. Actual public desktop/native integration and macOS/Windows CI coverage still remain R2-32. |

## Platform and feature comparison

This is a completion matrix for the reviewed concerns, not a claim that every possible OS API should have the same availability. Shared semantics must agree when a mechanism exists; unavailable observations stay unknown and OS-specific requirements remain explicit.

| Platform/host | Work verified or credited | Current blockers/gaps |
|---|---|---|
| Linux direct Node/Bun | Prior ordinary Connect ownership, immediate rejected admission rollback, dead-owner read completion and Pair retry controls pass; source assets and patch validate | R2-02–06 ownership; shared scan evidence; directories and acquired FD route; packed Bun integration |
| Windows direct Node/Bun | Real descriptor route, hard delivery selection, first CCCD and connectability mapping; low-level parameter mechanism added | Missing NAPI parameter forwarding; parameter ordering/validation/errors/faults/runtime capability; soft delivery preference; priority requests; directories, scan controls and Bun execution |
| macOS direct Node/Bun | Truthful unobserved ATT MTU; readiness and OS peer retrieval mechanisms; native compile lanes pass | Shared scan/GATT metadata concerns; actual Bun macOS packed qualification |
| React Native Apple | FIFO mobile scheduler; when-available connect; genuine CoreBluetooth readiness plumbing; truthful MTU | Incomplete internal connection/bridge methods; helper admission and source failure; OS-connected directory; primary/includes metadata |
| React Native Android | Existing mobile Rust tests and relevant public-boundary suites pass; ordinary Android controls remain available | Subrate36.1, encryption36/36.1, batching/PHY options, system-connected inventory and GATT metadata |
| Electron renderer | Shared IPC control streams and public typed source errors; retirement promises do settle correctly in the two disputed tests | writeWhenReady missing; NAPI parameter forwarding/underlying OS findings; stale test timing |
| Tauri | Control streams and restriction projection; macOS/Windows Rust checks pass | IPC helper, cumulative watcher quota, GATT metadata and current Linux formatting gate |
| Web Bluetooth | Existing web availability, lifecycle and backend source suites show PASS in the direct run; used as a contract comparator | No new concrete Web-specific defect promoted in this pass; browser-imposed feature restrictions remain genuine and are not Bun desktop qualification |

Genuine limits retained: Apple write capacity does not measure ATT MTU; Android has no equivalent Apple queue-readiness signal in the reviewed API surface; Apple service-scoped connected retrieval is a platform condition; Windows API build floors and Android minor SDK floors matter; permissions and controller/characteristic support remain necessary. Linux Device1.Connected may describe a BR/EDR bearer, so it cannot alone prove an LE link on a dual-mode peer.

Not promoted as defects: blanket BlueZ active/passive/PHY scan control (ordinary Adapter1 does not expose that whole set); Android measured connection-parameter callback at API 36/36.1 (current docs list 37.2 and its released/in-scope status was not established); unverified claims about every Windows reserved-service UUID/build; extra Bun loader incompatibilities not observed; peripheral/server roadmap items outside the current central/GATT scope.

## Validation actually performed

| Check | Final/current result | Scope and qualification |
|---|---|---|
| Current CI run 37558288591 | Completed with required job failures; both Android builds pass | Four JS lanes fail package tests; Windows Rust recording fixture times out; Linux Tauri rustfmt fails. All jobs are complete; the workflow is not green. |
| Linux Node 22 canonical package Jest | 7 failed suites,413 passed; 8 failed tests,5,279 passed |420 suites / 5,287 tests total; failures inspected individually. This supersedes the earlier prepack-blocked state. |
| Linux source-mode Bun 1.4.2 smoke |Pass |Actual staged native-addon smoke. |
| Linux packed Bun desktop acceptance |Pass |Exact installed identity, raw-native waker and generic public scenarios; current integration/OS limits remain R2-32. |
| macOS/Windows Bun CI |No such execution lane in this workflow |Not inferred from Linux imports; earlier manual evidence is not invalidated. |
| Classic RN and Expo CNG Android |Success |Both final-head builds completed; Apple compile job skipped. |
| Contracts, h10-sim, NAPI parity |Pass |h10-sim passes on 3 OSes. Rust workspace Linux/macOS and Tauri Windows/macOS also pass. |
| Final-head pnpm typecheck |Pass |Fresh isolated checkout, shared frozen dependency installation. |
| Final-head API-report check |Pass |24 TypeScript entrypoints checked. Standalone docs:check then stops at missing local generated build input; packaged CI gets beyond it. |
| Three directly changed JS suites | 66 pass across 3 suites |Canonical repository Jest config and guards; IPC cleanup, public control streams and scan evidence. |
| Final-head RN helper/readiness reproductions |Five defect scenarios still reproduced |Actual public factory/provider/binding with controlled native boundary; 13 assertions. |
| Final-head RN internal connection shape |Two defect tests pass on Android/Apple fixtures |Both methods are absent and direct calls throw TypeError; ordinary public controls still reject capability.unsupported. |
| Final-head IPC reproduction |Missing helper still reproduced; source-error positive control passes |One surviving defect and one verified closure. |
| Final-head scan and parameter reproductions |Four cache defects and two TS parameter defects still reproduced |Actual production algorithms/public facade with controlled observations; native NAPI blocker explicitly separated. |
| Focused NAPI forwarding guard | 1 failed, 49 passed |connection_parameters alone is missing from DispatchRadio; R2-34 is production default fallback. |
| Initial-head mobile Rust suite | 175 pass, 0 fail |23 unit + 152 integration. All relevant Rust bytes unchanged at final head; no rerun claimed. Current Windows fixture timeout still requires investigation. |
| Initial-head targeted Bun suites | 47 mobile/delivery + 36 public controls/runtime pass |Real diagnostic guard and minimal RN host-import preload. Source changes relevant to the mobile repro were rerun above. |
| Linux source/control evidence | 2/2 source assets; 7 fault scenarios; 6 positive controls |Exact patch/archive preparation;61 fingerprinted production functions. Relevant final-head bytes identical. No hardware or daemon execution claimed. |
| Correctly synchronized Electron controls | 2/2 pass at initial head |Production retirement promises settle. Binding source unchanged; router changes in final delta are formatting. Original CI timing assertions still fail. |
| Earlier broad local direct Jest | 98 PASS / 20 FAIL observed suite records; no complete aggregate |Noncanonical diagnostic run at initial head; prerequisite/environment triage preserved separately. Use final CI above for current complete package counts. |
| Full isolated BlueZ build |Blocked at missing pkg-config, exit 127 |Not a pass or a product-test failure. |

Review method: independent Linux ownership, Windows/native/Bun and shared/mobile audits plus a primary pass through scan semantics, IPC/Tauri projection, metadata, packaging and evidence. Suspected findings were tested or traced through the owning boundary and false-positive explanations were checked. Production sources remain unchanged; no review was posted and no daemon installed. CodeRabbit’s installer was rejected by automatic approval review because it attempted unverified PostHog telemetry egress; it was not retried or bypassed. This is the independent review and its stated evidence, not a CodeRabbit run.

## Required completion sequence

1. Repair the remaining CI gates and Linux ownership protocol first. Keep the previous positive controls while adding pending-Pair, pre-first-lease operation, live logical-retirement and no-peer physical-loss coverage.
2. Establish one operation/stream contract and apply it through desktop, RN, Electron and Tauri. Fix byte ownership/FIFO admission, RN source terminals, initial-event ordering, runtime capability truth, missing NAPI/RN methods and bounded retirement at their owning layers. Retain the newly corrected IPC public error projection.
3. Complete scan evidence with conservative ingress, per-fact freshness, full peer identity and bounded retention. Validate public queries through the actual native ingress rather than testing only the cache helper.
4. Propagate truthful GATT graphs and implement the listed OS-supported feature routes. A not-implemented limitation is not an OS limitation; an empty success is not an unknown fact. Regenerate strict wires/API reports from their authoritative sources after any further contract changes; the current report mismatch itself is fixed.
5. Run common public scenarios through every first-party host, then the actual packed Node/Bun desktop route on Linux/macOS/Windows. Qualify changed native behavior with physical evidence where needed; keep deterministic, ABI, packaging and radio claims separate.
6. Return a per-ID closure matrix with exact code, passing regression and gate/radio evidence. A test asserting the broken result in these reproductions is evidence of the current bug, not an acceptance test. Convert it to the intended invariant for the fix.

Do not address a finding merely by changing capability text, weakening the test, preserving a fabricated default, widening a quota or layering another platform-specific workaround. The goal is the correct shared behavior with explicit, genuine OS limits. No legacy compatibility shim is required by this review.

## Primary OS references

These sources support the API availability/behavior claims. Code links above are immutable to the reviewed SHA. BlueZ source provenance and licenses are included with the executable controls. Facts were checked on7 October 2026; use current runtime admission rather than assuming compiled target implies availability.

- [Microsoft: GetConnectionParameters](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.bluetoothledevice.getconnectionparameters?view=winrt-26100) — R2-17/24: disconnected all-zero observation; Windows 11 build 22000 API floor.
- [Microsoft: RequestPreferredConnectionParameters](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.bluetoothledevice.requestpreferredconnectionparameters?view=winrt-26100) — R2-23: preset request API and request-object semantics.
- [Microsoft: LE device enumeration selector](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.bluetoothledevice.getdeviceselector?view=winrt-26100) — R2-26: OS-visible LE enumeration subset.
- [Microsoft: connected-device selector](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.bluetoothledevice.getdeviceselectorfromconnectionstatus?view=winrt-26100) — R2-26: connection-status selector; Windows 10 build 10586.
- [Microsoft: typed address lookup](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.bluetoothledevice.frombluetoothaddressasync?view=winrt-26100) — R2-26: paired/system-cached visibility and absence; object lookup is not ownership of a link.
- [Microsoft: scan modes](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.advertisement.bluetoothlescanningmode?view=winrt-26100) — R2-31: Passive/Active and versioned None reception.
- [Microsoft: extended advertisements](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.advertisement.bluetoothleadvertisementwatcher.allowextendedadvertisements?view=winrt-26100) — R2-31: opt-in, default false, build 19041 floor.
- [Android: subrate request](https://developer.android.com/reference/android/bluetooth/BluetoothGatt#requestSubrateMode(int)) — R2-28: SDK 36.1; CONNECT plus companion association or privileged permission; native request status.
- [Android: subrate callback](https://developer.android.com/reference/android/bluetooth/BluetoothGattCallback#onSubrateChange(android.bluetooth.BluetoothGatt,int,int)) — R2-28: mode/status callback; not measured interval/latency/timeout.
- [Android: 16 QPR2 release](https://developer.android.com/blog/posts/android-16-qpr-2-is-released) — R2-28: released 2 December 2025; SDK_INT_FULL admission guidance.
- [Android: encryption observations](https://developer.android.com/reference/android/bluetooth/BluetoothDevice) — R2-29: ACTION_ENCRYPTION_CHANGE API 36, getEncryptionStatus36.1, transport and permission requirements.
- [Android: encryption status object](https://developer.android.com/reference/android/bluetooth/EncryptionStatus) — R2-29: encryption properties do not alone prove authentication or Secure Connections.
- [Android: scan settings](https://developer.android.com/reference/android/bluetooth/le/ScanSettings.Builder) — R2-30: report delay API 21; PHY API 26 with nonlegacy and hardware restrictions.
- [Android: system-connected profile inventory](https://developer.android.com/reference/android/bluetooth/BluetoothManager#getConnectedDevices(int)) — R2-26: system-wide GATT profile connections with normal CONNECT permission.
- [Apple: retrieving connected peripherals](https://developer.apple.com/library/archive/documentation/NetworkingInternetWeb/Conceptual/CoreBluetooth_concepts/BestPracticesForInteractingWithARemotePeripheralDevice/BestPracticesForInteractingWithARemotePeripheralDevice.html) — R2-26: system-connected/other-app service-scoped lookup requires subsequent local connect for GATT.
- [BlueZ5.87 official source archive](https://www.kernel.org/pub/linux/bluetooth/bluez-5.87.tar.xz) — R2-26/27 and Linux control provenance; verified archive SHA-256 in Linux control README.

## Bundle map

- `README.md`: quick entry and reproduction instructions.
- `REVIEW.md`: this complete report and source-linked findings.
- `AGENT_HANDOFF.md`: concrete instructions and closure format for the implementation agent.
- `findings.json`: structured priorities, origin, scope, evidence, source locations, correction and acceptance.
- `evidence/`: platform appendices, CI metadata, validation logs and rejected-false-positive explanation.
- `reproductions/`: portable source-level scripts and the fingerprinted Linux C control harness, with licenses.
- `SHA256SUMS`: content integrity of bundle files.
