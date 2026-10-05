# rc.19 — UBM port-review corrections

Status: Current release-branch tracker. Baseline: published rc.18 at
`bcb490a2cc96e5b1696a12db607caeaef1a95517`. This is one input review, not
the final release scope; later reviews join `release/5.0.0-rc.19`.

| Review item | UBM correction | Regression boundary |
| --- | --- | --- |
| 1 — migration | Current UBM 4.0.28-to-5.x guide shipped in the package; old ble-plx record explicitly historical/non-copyable; installation/restoration/capability/foreground-service recipes corrected | Consumer front-door links, version pin, package files and canonical packed-content gate |
| 2 — expert/export authority | `/advanced` listed in README and llms; AGENTS delegates to complete list; TV/background links present; Tauri has only generated signatures | Docs index/export coverage and generated API report checks |
| 3 — record-only iOS launch | Configured restoration bootstraps process host without native standing order; retain ASK authorization gate and tvOS refusal | Executable real Sessions bootstrap admission, startup identity policy, canonical Swift launch-observer compilation |
| 4 — Expo claim errors | Preserve normalized owner code/domain/platform/metadata/retryability/commit; shared Android no-source refusal also fixes host's earlier admission path | Real Expo Android no-source construction, Apple unavailable, structured failures and unknown failures |
| 5 — notifications | Document POST_NOTIFICATIONS as app-owned visibility permission, not foreground-service startup gate | Guidance checked against actual driver's required permissions |
| 6 — host recipes | Expo/bare TV construction and permissions distinguished; TVOS/GAPS historical; Tauri CCCD planning documented accurately | Executed Expo granted/denied recipe; doc checks against current native delivery planner |

One scoped CodeRabbit pass raised one minor ASK permission-recipe omission;
corrected in Getting Started and README/generated HTML with a focused regression.
No legacy IPC/error compatibility layer, new TV manager, Intel artifact,
speculative profile, dependency upgrade or backend-label promotion was added.
Bun-mono catalog and app-port changes are a separate downstream workstream.

## Second review — library and downstream boundaries

The second supplied review is pinned to the same rc.18 source. Its linked
detailed handoff was not attached; this tracker covers every item in the supplied
text, without claiming to have inspected that missing document.

| Item | Disposition | Completion evidence required |
| --- | --- | --- |
| U01 — Tauri capability truth | Corrected, including new U03 routes | All-catalog/per-OS native projection and routed RSSI/Service Changed pass; native projection regressions and TypeScript boundary tests passed; reference resolution uses current known or bonded authority without promoting unrelated directory capabilities; no physical-radio claim |
| U02 — ARM32 Fire TV | Three-ABI producer, packed TV ARM32 and classic RN graphs pass | Real ARM32 Rust/JNI identity/hash/alignment and 49 JNI exports verified; Linux Gradle fixture gate rejects 11 malformed objects; packed Expo 57 / RN-TV 0.86 app builds with 19 ELF32 ARM libraries; classic RN builds ARM32/ARM64 with 15 libraries each; complete dependency closure verified; physical ARM32 BLE qualification remains open |
| U03 — desktop mechanisms | Selected mechanisms implemented; full consumer/qualification gates pending | Typed Windows address resolution, Windows/Linux bonded enumeration, macOS/Windows native deferred acquisition with cancellation/deadline/retained cleanup and Node/Tauri parity; 304 desktop, 155 Tauri and 198 fresh-addon consumer tests pass; CoreBluetooth unrestricted bond inventory and Linux LE-specific deferred availability remain explicitly unsupported |
| Q01 — maintained Linux deployment | Existing explicit deployment/rollback boundary; receipts and coverage being audited | Actual deployed daemon scan/connect/discover/stream, cancellation, reconnect, second-client protection, adapter loss and owner replacement; reuse only identifiable unchanged receipts |
| Tauri long-write guidance | Corrected test-first | Distinguish OS-managed ordinary with-response writes from unavailable caller-controlled prepared/reliable transactions |
| TVOS/PLATFORMS drift | Addressed in first patch | Current TV guide/factory/permission documentation and historical markers |
| Artifact-bound support | Open qualification boundary, not inferred from a version | Existing evidence schema and exact source/artifact receipts; no synthetic hardware promotion |

One bounded independent U03 review produced three corrective findings, handled
as one batch rather than repeated broad review rounds:

| Finding | Disposition | Regression |
| --- | --- | --- |
| R01 — Windows cleanup short-circuit | Corrected | Refused local handler cleanup does not skip authoritative peripheral release; failed obligations remain retryable; focused regression passes |
| R02 — typed Windows cache race | Corrected | Concurrent same-address public/random lookup shares one atomically inserted identity and reports one type conflict; controlled concurrency regression passes |
| R03 — Tauri foreign reference | Corrected | Bonded query rejects the wrong backend before native dispatch; same-backend filtered query remains positive; 13 focused directory tests pass |

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

Read-only inspection confirms lx5090 is running the maintained
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

The maintained `.3` daemon does not expose a bearer-specific peer-availability
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
pass. The full package suite and clean detached integrated preflight remain pending.

The first full local package run passed 408 suites and failed three (8 assertions):
the new ARM32 commit-admission guard, stale two-ABI tarball fixtures and the old
CoreBluetooth TCK feature list. The commit-admission rule and all tarball fixture
negatives are now corrected and pass focused retests; the TCK correction includes
an actual deferred-intent dispatch assertion, not merely a new string. All five
affected/reference-roundtrip suites pass 64 focused tests.
The next clean integrated preflight will establish the complete final result.
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
