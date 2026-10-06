# rc.19 — UBM port-review corrections

Status: Historical. Shipped as immutable `v5.0.0-rc.19` on 2026-10-05 from
`f2e98f41e0d416e6abc6a33594b9d445e8c722b6`. The body below is the retained
pre-publication record, not current release instructions. Open physical
qualification and downstream handoff items are not claimed complete.

Original baseline: published rc.18 at
`bcb490a2cc96e5b1696a12db607caeaef1a95517`. This is one input review, not
the final release scope; later reviews join `release/5.0.0-rc.19`.

| Review item                 | UBM correction                                                                                                                                                                          | Regression boundary                                                                                                |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| 1 — migration               | Current UBM 4.0.28-to-5.x guide shipped in the package; old ble-plx record explicitly historical/non-copyable; installation/restoration/capability/foreground-service recipes corrected | Consumer front-door links, version pin, package files and canonical packed-content gate                            |
| 2 — expert/export authority | `/advanced` listed in README and llms; AGENTS delegates to complete list; TV/background links present; Tauri has only generated signatures                                              | Docs index/export coverage and generated API report checks                                                         |
| 3 — record-only iOS launch  | Configured restoration bootstraps process host without native standing order; retain ASK authorization gate and tvOS refusal                                                            | Executable real Sessions bootstrap admission, startup identity policy, canonical Swift launch-observer compilation |
| 4 — Expo claim errors       | Preserve normalized owner code/domain/platform/metadata/retryability/commit; shared Android no-source refusal also fixes host's earlier admission path                                  | Real Expo Android no-source construction, Apple unavailable, structured failures and unknown failures              |
| 5 — notifications           | Document POST_NOTIFICATIONS as app-owned visibility permission, not foreground-service startup gate                                                                                     | Guidance checked against actual driver's required permissions                                                      |
| 6 — host recipes            | Expo/bare TV construction and permissions distinguished; TVOS/GAPS historical; Tauri CCCD planning documented accurately                                                                | Executed Expo granted/denied recipe; doc checks against current native delivery planner                            |

One scoped CodeRabbit pass raised one minor ASK permission-recipe omission;
corrected in Getting Started and README/generated HTML with a focused regression.
No legacy IPC/error compatibility layer, new TV manager, Intel artifact,
speculative profile, dependency upgrade or backend-label promotion was added.
Bun-mono catalog and app-port changes are a separate downstream workstream.

## Second review — library and downstream boundaries

The second supplied review is pinned to the same rc.18 source. Its linked
detailed handoff was not attached; this tracker covers every item in the supplied
text, without claiming to have inspected that missing document.

| Item                              | Disposition                                                           | Completion evidence required                                                                                                                                                                                                                                                                                                                   |
| --------------------------------- | --------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| U01 — Tauri capability truth      | Corrected, including new U03 routes                                   | All-catalog/per-OS native projection and routed RSSI/Service Changed pass; native projection regressions and TypeScript boundary tests passed; reference resolution uses current known or bonded authority without promoting unrelated directory capabilities; no physical-radio claim                                                         |
| U02 — ARM32 Fire TV               | Three-ABI producer, packed TV ARM32 and classic RN graphs pass        | Real ARM32 Rust/JNI identity/hash/alignment and 49 JNI exports verified; Linux Gradle fixture gate rejects 11 malformed objects; packed Expo 57 / RN-TV 0.86 app builds with 19 ELF32 ARM libraries; classic RN builds ARM32/ARM64 with 15 libraries each; complete dependency closure verified; physical ARM32 BLE qualification remains open |
| U03 — desktop mechanisms          | Selected mechanisms implemented; exact-head cross-platform CI pending | Typed Windows address resolution, Windows/Linux bonded enumeration, macOS/Windows native deferred acquisition with cancellation/deadline/retained cleanup and Node/Tauri parity; clean detached consumer/Tauri gates pass; CoreBluetooth unrestricted bond inventory and Linux LE-specific deferred availability remain explicitly unsupported |
| Q01 — maintained Linux deployment | Approved `.4` deployment and focused radio checks pass                | Service-filtered scan/connect/discover/stream, pending acquisition cancellation, repeated reconnect, second-client protection, adapter loss and daemon replacement pass on identified sources/artifacts; final integrated CI remains required                                                                                                  |
| Tauri long-write guidance         | Corrected test-first                                                  | Distinguish OS-managed ordinary with-response writes from unavailable caller-controlled prepared/reliable transactions                                                                                                                                                                                                                         |
| TVOS/PLATFORMS drift              | Addressed in first patch                                              | Current TV guide/factory/permission documentation and historical markers                                                                                                                                                                                                                                                                       |
| Artifact-bound support            | Open qualification boundary, not inferred from a version              | Existing evidence schema and exact source/artifact receipts; no synthetic hardware promotion                                                                                                                                                                                                                                                   |

One bounded independent U03 review produced three corrective findings, handled
as one batch rather than repeated broad review rounds:

| Finding                             | Disposition | Regression                                                                                                                                                   |
| ----------------------------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| R01 — Windows cleanup short-circuit | Corrected   | Refused local handler cleanup does not skip authoritative peripheral release; failed obligations remain retryable; focused regression passes                 |
| R02 — typed Windows cache race      | Corrected   | Concurrent same-address public/random lookup shares one atomically inserted identity and reports one type conflict; controlled concurrency regression passes |
| R03 — Tauri foreign reference       | Corrected   | Bonded query rejects the wrong backend before native dispatch; same-backend filtered query remains positive; 13 focused directory tests pass                 |

Final integration also checks the front-door operation, not just inventory:
new Windows/Linux bonded references must roundtrip through public
`connect(reference)` without promoting unrelated peer-directory capabilities.
That route correction passes for Node and Tauri: current bonded inventory
resolves the saved reference while preserving `system-bonded`/`bonded`, and a
removed bond resolves to `null`. Public Node tests exercise actual synthetic
native connection ownership with only the read-only inventory boundary doubled;
Tauri tests exercise inventory, resolution and the scoped connection route.
Seven fresh-addon consumer/document suites pass 221 tests. This is not physical
Windows/Linux evidence. The macOS production build caught a
Windows-only cleanup helper referenced outside its compile guard; the guard is
corrected and the real macOS production `cargo check` passes. Final artifact
refresh follows settled source and the final isolated Linux owner-race regression.

Downstream handoff after the corrected UBM artifact: A01 consuming Cargo patches
and toolchain (preserve Clerk patch); A02/A03 receipt-aware CGM/mobile cleanup and
same-handle retry; A04/A05 tvOS admission and lifecycle-event loss monitoring;
A06 Windows/Linux credential storage; A07 remove the internal Vite rewrite;
A08 public permission/readiness flow preserving ASK behavior. Catalog/lockfile
and representative native application builds belong to bun-mono integration,
not this library patch. They are tracked, not claimed fixed in UBM.

### Linux receipt audit

Initial read-only inspection confirmed lx5090 was running the maintained
`5.87-ubm.3-824a9e2c8572` deployment with zero systemd restarts. Existing `.3`
cutover and public Electron/Tauri scan/GATT/read receipts remain retained.
The actual deployed executable SHA-256 is
`d6cdbe85a1e31c9dd69671a00ca66023331c2fc5fcbfa7b3036354e3491c8913`;
both adapters answer the real `(1,2,1)` authority contract. The current central
is hci0 and simulator is hci1, reversed from the historical two-client harness.
The older `.2` physical shared-owner/Classic-continuity receipt proves only its
identified daemon/addon combination; it is not silently relabeled as `.3` or
as a published rc.19 artifact. Cancellation, repeated reconnect, adapter loss,
owner replacement and second-client protection must be checked against the
final changed Linux implementation, after its native source is frozen. Private
D-Bus coverage is retained separately from actual deployed-radio outcomes.

The maintained daemon contract does not expose a bearer-specific peer-availability
event. Stock Device1 RSSI/manufacturer/service-data changes can include Classic
inquiry when discovery filters merge across clients. Waiting for those signals
and then attempting LE is not the same native deferred-acquisition mechanism;
this patch does not advertise that substitution as `when-available`. The Linux
mechanism remains an explicit gap, not a claim that Linux can never support it.

The bonded-snapshot pre/post owner fence passes an exact private-bus regression:
a held GetManagedObjects reply spans unique-owner replacement, the old authority
refuses the reply without rebinding or querying the replacement, and a fresh
authority reads the replacement's bonded inventory. The focused test passes
1/1 and the existing private-bus library gate passes 18/18. This controlled
service test is separate from actual deployed-radio qualification. Windows native
I/O remains compile/model evidence. Transient read-only Windows lookup objects
report Close failures and then release their COM scope; no maintained connection
or GATT lease is acquired by those queries. That is not a claim that Close
succeeded. Maintained-session cleanup failures remain explicitly owned for retry.

## Evidence limits

The injected refusing installer test proves real record-only startup admission
and visible failure, not actual CBCentralManager allocation. Swift compilation
and ASK startup policy checks are not physical radio evidence. A configured
record-only physical cold-relaunch receipt remains to be obtained for this
runtime change. Existing timed picker-return/company-only, user-force-quit and
physical Apple TV gaps are unchanged. No phone was accessed for this patch.

## Validation

The second patch's final canonical Android refresh produced all three maintained
ABIs from JNI source digest
`0aea78417bf31f808371b096dca08f874c4e6ef51cd50a24dd1f69b695f49052`;
all 60 offline guards pass. The scoped outside-repository packed Expo TV ARM32 consumer passed with
tarball SHA-256 `5a9c1a45b5407d77959fbd4786633c92b11ff89325294aecf111e63fcf57707f`
and APK SHA-256 `d2b39191131c804d6a727f96879ff2825233ec2a37d4ccd7dcf688c6254477c1`.
The final inspector validates full ELF headers and DT_NEEDED dependency closure;
API 30 coarse/fine permissions are present in the actual APK manifest. This is
targeted compile/packed-native evidence, not the complete shipping matrix or
physical Fire TV qualification. Android offline artifact guards pass 60 tests;
Apple's final canonical refresh verifies four ARM64 slices; the final Mac addon
also passes 166 targeted tests across five affected suites, including public
bonded-reference connection and deferred-intent dispatch. Local plugin, lint,
evidence, native status, dependency artifacts and regenerated API/docs checks
pass. The final clean detached preflight at `82a63d1bb41b8fa9fa60f11f2ea815ad1a846f85`
passed in 556 seconds: 411 package suites / 5,161 tests, 67 plugin tests,
156 Tauri library tests, private-bus suites and outside-repository packed consumers.
The later dead-owner cleanup correction requires its own changed-source gates;
these results are not silently reassigned to that correction.

PR #248's exact-head CI at `82a63d1` found one stale Windows capability-test
expectation: it still admitted address targeting only on Linux. Windows's
implemented typed lookup correctly projected `ProceedWithLimitation`. The
regression now requires that exact admission on both implemented platforms and
retains macOS's `CapabilityUnsupported` assertion. The failed run is retained;
it is not represented as a green cross-platform pass. A new integrated head
must pass CI after this correction and the Linux owner-retirement fix.

The first full local package run passed 408 suites and failed three (8 assertions):
the new ARM32 commit-admission guard, stale two-ABI tarball fixtures and the old
CoreBluetooth TCK feature list. The commit-admission rule and all tarball fixture
negatives are now corrected and pass focused retests; the TCK correction includes
an actual deferred-intent dispatch assertion, not merely a new string. All five
affected/reference-roundtrip suites pass 64 focused tests.
The clean integrated preflight above established the complete result for its
identified source, without erasing the initial failures.
The native review corrections invalidate the initial frozen source identities;
their refreshed final artifacts must pass before these earlier compile receipts
can be treated as evidence for unchanged mechanisms, not the final package bytes.

The classic RN ARM32/ARM64 APK also passed compilation and both ABI graph checks,
SHA-256 `244fe9fe8a110f5efd724cdfb147b348b8a69649dbd7cdac8d1365f7c19a6b92`.
Its first dependency installation failed on a temporarily missing prepared
declaration file; the unchanged canonical installation retry and build passed.
That retry is retained, not presented as a first-attempt pass. The Linux snapshot
transfer initially included Mac AppleDouble metadata; only verified transfer
sidecars were removed from the isolated test copy before unchanged prepack passed.

Focused regressions, canonical Apple native protocol, lint/typecheck, evidence
validation, prepack, 67 plugin tests, native status, generated docs and dependency
artifact checks passed locally. The first patch's exact clean detached Linux
preflight passed at `1e9de07412c51a406759b06c742b96f932f6bd0d`: 409 package
suites / 5,138 tests, 67 plugin tests and both package/Tauri lanes, including
external packed consumers, in 596 seconds. `--fast` excludes Android Gradle and
Expo reference typechecking; Apple, Windows and the full shipping matrix remain
CI boundaries. [PR #247](https://github.com/sfourdrinier/unified-ble-manager/pull/247)
passed exact-head CI run `37263329709`, including all three Apple checks, and
merged into the release branch at `ad33ea66f562aef527ac11ffb19b4a59037e2de2`.
The eventual integrated
release branch must pass its own gates before
one PR to main; this tracker does not authorize publication or stable promotion.

The second patch's first exact clean preflight at `d2272479` failed the canonical
Rust formatting gate before compilation. Per-file formatting had used the wrong
edition for the NAPI/Tauri files. The pinned Cargo formatter corrects four files;
both workspace and Tauri formatting checks then pass. Android and Apple source
identities remain unchanged; only the desktop binding needs canonical refresh.
The corrected commit must pass a new exact clean preflight before push. This
format-only change does not trigger another APK or physical-radio campaign.

The next exact clean run at `818a654b` passed the formatting prerequisites but
found a Tauri resolver regression: reference decoding selected the bonded OS
backend before choosing the actual known/bonded mechanism. Decoding now follows
that selected mechanism, retaining foreign-reference refusal before a native
directory query. The full Linux Tauri library suite passes 156/156 and Mac
directory tests pass 13/13. Desktop/mobile native identities are unchanged.

The separate maintained `.3` physical Linux run encountered a real SIGSEGV in
`ubm_watch_current` during failed GATT discovery/disconnection. Crash evidence
identifies a stale watch callback; the extension's ready-registration ID was
truncated from a 64-bit pointer, preventing unregister from removing the queued
callback before its user data was freed. The `.4` correction uses monotonic
per-client IDs, independent watch references, owner retirement before unregister,
and deferred destruction until a running callback returns. Actual native tests
cover 64-bit unregister, zero/exhaustion, queued already-ready registration,
reentrant cleanup and late callbacks. Fresh extraction of the generated patch
passes the canonical isolated daemon producer gate; one independent bounded
review found zero actionable issues and reran both focused native executables.
Patch SHA-256 is
`f227176d86a045cca4df371971972763e3747f4a06e047e9e3e4c0e4d533afb9`.
The authority contract stays `(1,2,1)`. At that stage no running daemon was replaced.
Services were restored after the failed test. The simulator exited in the same
window, but its termination reason remains unverified. No Linux qualification
pass or release readiness is claimed for the failed run; the bounded deployed
radio rerun required explicit `.4` cutover approval.

The user subsequently authorized cutover on lx5090. The sealed `.4` daemon
SHA-256 `4288da8f14534465280296f874f5e746a13d60bbd6deb6e10713df6a52d8b96b`
was installed through the existing explicit host deployment boundary; the `.3`
prefix and prior service override were retained for rollback. The actual Linux
addon SHA-256 was `16bbf062ac9aa6b6f7052b61a841c978d9745114f59ab8dc5747ef652deef084`.
The previous failed-discovery cleanup no longer crashed `.4`. An asymmetric
simulator bond caused an independently diagnosed SMP authentication failure;
only the dedicated test peer's record was removed after a root-only backup.
A temporary pairing agent authorized only the two dedicated adapter peers and
was retired after testing. Unrelated devices and rtx3090 were untouched.

Focused physical checks then passed: service-filtered discovery and positive
72 bpm heart-rate delivery, pending public acquisition abort followed by recovery,
two repeated fresh acquisitions, two independent clients with protected release
preserving the other's fresh data, final physical release and retained-handle
retry, plus actual adapter interruption and restoration. These are two-adapter
simulator-radio results, not a real Polar-device, phone or shipped-package claim.

Graceful daemon replacement first produced the actual `adapter-loss` event when
the service powered down its adapters; it was not relabeled `backend-restart`.
The old authority did not rebind, and a fresh authority delivered new values.
However, old-manager cleanup repeatedly returned `release-failed` for the
conclusively vanished unique daemon owner. The bounded correction checks the
original pinned unique owner's lifetime with the bus daemon. Only a confirmed
absence retires that owner's lease/reservation/acknowledgment obligations; it
does not fabricate an ACL generation or reason or retire arbitrary local cleanup.
Live-owner refusal, misleading error names and failed lifetime queries retain
ownership. All 28 ledger regressions and an actual private-bus lifetime test pass.

The focused physical reproduction now passes on desktop source digest
`c67f0a542bd3608752d0edf0d0ad59bbb786769e10ca43edc0abeb84db79580b`,
schema `ec99669af6af60b703ca48c0fa791aa54efeb829a27f61d1c7154d59612ec5a1`,
and Linux addon SHA-256
`3af78b19f7ea223c18a431d00d64799cea223563c2b555a41d8b167027790ef9`.
After owner `:1.1406` was replaced by `:1.1428`, old-manager destruction released
on its first attempt with no failures. A new manager then acquired the
service-filtered simulator, discovered fresh GATT, received three 72 bpm values,
disconnected with the observed reason and destroyed successfully. The actual
graceful-stop terminal remains `adapter-loss`, not an invented restart reason.

Receipts are retained under `/tmp/ubm-rc19-final-q01.jw82pM` on lx5090:
`owner-fixed.log`, `new-owner-final-positive.log` and
`dead-owner-prepack-current-jni.log`, alongside the original failed attempts.
Both adapters were restored powered on, WirePlumber and fwupd were restored
active, and the temporary pairing agent was retired. The simulator remains an
explicit test fixture. Unchanged successful physical scenarios were not rerun
for this isolated correction or documentation edits.

PR review identified capability receipts bound to unrelated scan/RSSI scenarios.
The corrected suites now run `peer.bonded-enumeration-preserves-native-facts`
and `connection.when-available-acquires-and-releases`. The original bindings
reproduced seven failed expectations, including false passes for empty bonded
inventory and refused deferred acquisition; the correction passes all 14
focused registration tests. Bond-store input is an explicitly deterministic OS
inventory double, while native connection acquisition and teardown execute
through the actual synthetic DesktopCentral. This does not promote physical
evidence. Final local desktop tests pass 309/309; Linux ledger tests pass 28/28
and actual private-bus ownership tests pass 19/19. Final integrated preflight
and new-head cross-platform CI remain required.

The subsequent exact-head PR review at `41d60982` found that a cancelled release
waiter could retain a confirmed release while a retry queried daemon lifetime
before returning it. A transient query failure then incorrectly reversed that
confirmed fact. The strengthened cancelled-waiter regression reproduces the
failure and now verifies the exact retained generation/reason with no additional
owner query or native release. The narrow correction returns `State::Released`
first; acknowledgment maintenance remains independently owned. All 28 ledger
tests, strict clippy and the canonical private-bus/source-producer gate pass.
CodeRabbit's bounded review of the single Rust file raised zero issues. The
canonical native refresh rebuilt and verified all maintained local consumers;
new exact-source gates remain required. Prior physical receipts are not silently
reassigned to this correction.

Final remediation head `444322e4eb672a61aaa17ae125165ce7989c21fe` also corrects
the Tauri crate's bonded-directory guide with a test-first, nine-test regression.
Its clean detached Linux preflight passes: 411 package suites / 5,168 tests,
67 plugin tests, Tauri and the native/private-bus and packed-consumer gates.
Canonical CI `37343140098` passes all 21 jobs, including the three Apple and
both Android builds. PR #248 merged only into `release/5.0.0-rc.19` at
`5cf05f05137f00fcaa26d13450a6040668d4dee5`; its tree is identical to that tested
head. Integrated release-branch CI and rc.19 identity preparation remain pending.
Main and published rc.18 are unchanged; physical qualification gaps above remain
open and these compile/deterministic results do not promote evidence labels.

Integrated CI exposed a timing-dependent mobile test fixture: its 20 ms deadline
could expire before radio dispatch while the test assumed post-dispatch orphan
cleanup. Delayed admission reproduced the exact failure with zero physical
requests. The test now synchronizes on admitted scan cancellation, preserving
the refused-first-stop and confirmed retry assertions; a separate queued-expiry
test requires zero start/stop requests. All nine tests and five repeated batches
pass, with strict clippy and formatting. No runtime code or native artifact
changed. The failed integrated run remains retained, not relabeled successful.
