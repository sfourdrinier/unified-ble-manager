# PR #251 rc.21 remediation

Status: Current working record. No item is closed merely because its implementation changed.

Review baseline: `b80542f39144949738b04dc7d19b4a7ee52cc8c6`.
Remediation baseline: `a94bee1e3c875c3471000f26b42555776d74f5b3`.
The intervening commit changes only Tauri import formatting.

The supplied review tracks 35 items: 33 open and two fixed. This record preserves
that denominator and distinguishes defects, feature omissions, and qualification.
The supplied ZIP has been extracted and every deliverable hash verified. Its
immutable review, instructions, and structured findings are retained under
`rc21-round2/`. `baseline-source-map.json` binds all 105 referenced locations to
reviewed and remediation-baseline git blobs. Only the Tauri import formatting
differs. Baseline execution logs are retained under `rc21-round2/baseline-logs/`
and at `/tmp/ubm-rc21-verification/baseline`;
they establish reproduced defects, not successful fix verification.

The owner confirmed that every item and all feedback are in scope. The newest
automated IPC parameter-result identity finding is an additional obligation;
it does not replace or shrink the original 35-item review denominator.

## Closure ledger

| Item  | Priority | Required outcome                                                                                  | Verification status                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ----- | -------- | ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R2-01 | P1       | All required CI and package gates pass on the final head; investigate the Windows setup timeout   | Baseline CI confirms eight package failures; supported-parameter TCK now invokes and validates the operation in source; combined gate and old timeout investigation remain pending                                                                                                                                                                                                                                                     |
| R2-02 | P1       | Protect accepted pending foreign Pair for its full lifetime; rollback rejected work               | Baseline Linux production-function control reproduces inappropriate disconnect request; pending Pair/generation admission and lifetime rollback source plus controls written; frozen-batch and native daemon/physical qualification pending                                                                                                                                                                                            |
| R2-03 | P2       | Retain reconciliation ownership for protected logical lease release and reclaim daemon tokens     | Both baseline controls reproduce; revision-3 producer/consumer protected ACK and generation-owned reconciliation written; private-bus regression exceeds 1024 live-sender cycles and covers disconnect refusal/retry; frozen-batch and real daemon/controller qualification pending                                                                                                                                                    |
| R2-04 | P2       | Abandon admission when deferred acquired-FD delivery fails, preserving other interest             | Baseline deferred AcquireWrite control reproduces phantom admission; exact pending write/notify retirement and ready-callback removal written; native fixture covers socket/readiness/reply failure, actual notify-registration cleanup, StopNotify, characteristic disposal, sender death and successful FD delivery; frozen-batch and physical qualification pending                                                                 |
| R2-05 | P2       | Track foreign one-shot operations before the first UBM lease                                      | Baseline Linux control reproduces unprotected pre-lease read; pre-first-lease accepted-operation ownership source and controls written; frozen-batch and native/physical qualification pending                                                                                                                                                                                                                                         |
| R2-06 | P2       | Clean pre-lease arrival/admission state on physical loss                                          | Baseline Linux controls reproduce retained arrivals and cross-generation admission; no-peer physical-end retirement and repeated-generation controls written; frozen-batch verification pending                                                                                                                                                                                                                                        |
| R2-07 | P2       | RN ready writes own bytes on admission, preserve FIFO/deadline, and settle on invalidation        | Immutable-baseline public RN reproductions confirmed buffer mutation, order inversion and invalidation hang. Native FIFO admission, synchronous mobile/NAPI byte ownership, native readiness wait, budget/cancellation/database/source/loss regressions and RN/desktop integration written; delayed-worker generation fences, cleanup interaction audit and complete-batch verification pending                                        |
| R2-08 | P2       | RN readiness source failures terminalize independently of cleanup                                 | Baseline source failure and cleanup failure reproduce; terminal delivery, held-opening cancellation, original failure retention and later-acquisition refusal written with regressions; frozen-batch verification pending                                                                                                                                                                                                              |
| R2-09 | P2       | IPC characteristic writeWhenReady uses the supported readiness route                              | Immutable-baseline public IPC reproduction confirmed readiness true followed by unsupported helper and zero native writes. Public IPC/Electron/Tauri native-owned helper routes and acceptance/byte-ownership regressions written; queue/admission audit and complete-batch verification pending                                                                                                                                       |
| R2-10 | P2       | Preserve public IPC control-stream errors                                                         | Review reports fixed; retained regression re-verification pending                                                                                                                                                                                                                                                                                                                                                                      |
| R2-11 | P2       | Live-watch quotas and bounded replay history remain independent                                   | Source-confirmed; 600-cycle real-dispatcher regression plus concurrent quota coverage and bounded history correction written; verification pending                                                                                                                                                                                                                                                                                     |
| R2-12 | P2       | Union keyed list evidence and retain required packets through native ingress                      | Baseline reproduces split-list failure; keyed union and conservative WinRT ingress written; full pipeline and Windows verification pending                                                                                                                                                                                                                                                                                             |
| R2-13 | P2       | Expire each carried scan fact independently                                                       | Baseline reproduces aggregate timestamp refresh; per-fact expiry and downstream merged-projection nonrenewal regressions written; verification pending                                                                                                                                                                                                                                                                                 |
| R2-14 | P2       | Complete scan peer identity prevents cross-peer evidence borrowing                                | Complete-identity cache, immutable WinRT public/random peer identities, captured BlueZ/WinRT address type through NAPI/provider/Tauri, and identity controls written; unread-identity native failure metadata and full pipeline execution pending                                                                                                                                                                                      |
| R2-15 | P2       | Expire inactive scan peers globally and bound retention                                           | Baseline reproduces expired unbounded peer retention; per-fact global expiry, bounded peer/fact admission and capacity regressions written; frozen-batch execution pending                                                                                                                                                                                                                                                             |
| R2-16 | P2       | Parameter initial probes cannot overwrite newer events                                            | Baseline reproduces stale publication; probe buffering and acquisition failure regressions written; verification pending                                                                                                                                                                                                                                                                                                               |
| R2-17 | P2       | Snapshot and streamed parameters share fail-closed validation                                     | Baseline reproduces inconsistent zero handling; shared guard and malformed probe/stream regressions written; verification pending                                                                                                                                                                                                                                                                                                      |
| R2-18 | P2       | Lost-link errors retain shared vocabulary for parameter operations                                | Source-confirmed parameter operation missing from link classifier; classifier correction and native/shared-vocabulary regressions written; frozen-batch execution pending                                                                                                                                                                                                                                                              |
| R2-19 | P2       | Forward supported soft delivery preference distinctly from hard requirement                       | Separate provider/NAPI/core/radio preference route and first-write selection written; supported-only fallback and hard requirement regressions written; native execution and qualification pending                                                                                                                                                                                                                                     |
| R2-20 | P2       | Preserve structured WinRT primary and cleanup failures and retry cleanup                          | Initial discovery now uses the structured status/ATT mapper; CCCD rollback keeps typed primary and cleanup errors and the original retryable target; controlled combined-failure/retry regression written; frozen-batch and WinRT execution pending                                                                                                                                                                                    |
| R2-21 | P2       | Propagate Tauri service restrictions                                                              | Review reports fixed; retained regression re-verification pending                                                                                                                                                                                                                                                                                                                                                                      |
| R2-22 | P2       | Carry observed primary/included-service graph facts and occurrence identity                       | Shared Rust graph and NAPI/JNI/UniFFI/mobile/IPC/Tauri projections written; BlueZ property identities and WinRT/Apple included-service traversal in progress; public unknown/secondary/duplicate-inclusion and joined three-profile native-source-to-public occurrence remapping regressions written. Native failure/stale callback audits, legacy/Web metadata paths, generated binding refresh and frozen-batch verification pending |
| R2-23 | P2       | Complete Windows preferred parameter request route and lifetime                                   | Native request ownership, bounded replacement/teardown cleanup debt, runtime API admission and Rust/NAPI/provider/Electron/Tauri routes written; joined synthetic public/IPC and core lease/cancel/deadline/post-release regressions added; native status/fault lifetime acceptance audit, frozen-batch execution and Windows qualification pending                                                                                    |
| R2-24 | P2       | Runtime WinRT capabilities reflect actual API presence                                            | Native open probes the WinRT getter/event API through NAPI dispatch; absent APIs override the descriptor to unavailable and probe errors remain structured failures; regression execution and Windows qualification pending                                                                                                                                                                                                            |
| R2-25 | P2       | Surface callback getter failure and reconcile early parameter queue lag                           | Getter errors and native broadcast gaps now cross the generation-bound core/NAPI/Tauri event routes; source failure cancels initial acquisition while preserving its cause, and gaps trigger Node/Bun provider reconciliation; controlled joined-route regressions written, callback fault injection and platform verification pending                                                                                                 |
| R2-26 | P2       | Implement available system/known peer directories independent of manager cache                    | Source-confirmed omission; distinct Linux owner-fenced known/LE-connected inventory, Windows selector/default-adapter guard/typed identities/retryable transient-object cleanup, desktop NAPI/provider routes and new regressions written. Mobile and Tauri native inventories/foreign reference routes and regressions written; runtime subset audits and full verification remain pending                                            |
| R2-27 | P2       | Implement eligible BlueZ acquired FD transports with owned lifetime and backpressure              | Native FD/sender ownership, central/NAPI/provider/public sessions and source/private-bus/packet-socket regressions written; Electron/Tauri IPC source routes and ownership regressions written; remaining race audits, batch verification and Linux Node/Bun physical qualification pending                                                                                                                                            |
| R2-28 | P2       | Implement Android SDK 36.1 subrate with runtime authorization                                     | Current API verified; public/provider/Rust/JNI/Kotlin route, native status preservation, runtime probe and regressions written; generated refresh, execution and physical qualification pending                                                                                                                                                                                                                                        |
| R2-29 | P2       | Implement Android encryption event/snapshot observations without invented authentication          | Current API verified; native LE event/snapshot projection, generation retirement, typed failures/reconciliation and regressions written; frozen-batch execution, native broadcast correlation audit and physical qualification pending                                                                                                                                                                                                 |
| R2-30 | P2       | Implement available Android batching/PHY scan options and batch intake                            | Current Android API verified; regressions and public/wire/JNI/UniFFI/native forwarding plus batch intake written; generated binding refresh, native batch execution and qualification pending                                                                                                                                                                                                                                          |
| R2-31 | P2       | Implement available Windows scan mode and extended advertisement options                          | Typed public modes, NAPI/native and Electron/Tauri propagation, runtime enum/property and default-adapter support admission, per-scan default reset, portable refusal-profile and joined synthetic public regressions written; native extended payload and ownership-failure acceptance audits, frozen-batch and Windows qualification pending                                                                                         |
| R2-32 | P2       | Qualify the actual packed public desktop provider/native integration under Node/Bun on three OSes | Joined installed public factory/default provider/sealed native fixture written for CJS/ESM Node/Bun; Windows parameter read/watch forwarding included; three-OS CI matrix written. No new qualifier or same-head matrix execution yet                                                                                                                                                                                                  |
| R2-33 | P3       | Scan planner reports native connectability accurately                                             | Source-confirmed omission; profile correction and regression written; verification pending                                                                                                                                                                                                                                                                                                                                             |
| R2-34 | P2       | Forward native parameters through real and synthetic NAPI radios; test joined read/watch route    | Existing forwarding failure confirmed; forwarding and joined public/provider/addon read-watch regression written; verification pending                                                                                                                                                                                                                                                                                                 |
| R2-35 | P2       | Implement RN internal parameter methods and bridge shapes together                                | Baseline Android/Apple factory reproductions confirm both methods undefined with an incomplete accepted shape; RN internal methods and production bridge table corrected, with both factory/public-guard controls written; frozen-batch verification pending                                                                                                                                                                           |

## Verification policy

Baseline reproductions executed against the clean detached a94bee1e checkout:
scan evidence, IPC helper/R2-10 positive control, parameter ordering, parameter
numeric validation, Android/Apple RN connection shapes, and all five RN readiness
failure scenarios reproduce as described. The Linux controls initially could
not compile on macOS because Linux socket flag definitions are absent there.
They subsequently ran on the configured `lx5090` Linux host: all seven fault
scenarios reproduced and all six positive controls passed.
`linux-controls-lx5090.log` retains the output. These execute complete production
functions against controlled boundaries; they demonstrate control-flow decisions,
not measured controller teardown or physical-radio qualification.

Read-only lab inventory found a configured Linux host with BlueZ build dependencies,
a running `5.87-ubm.9` daemon, one host controller and a simulated Polar H10
controller. That simulation is not a physical peripheral receipt. Local mobile
tools expose iOS 18.2/26.5 simulators and an Android API 36.0 emulator, with no
physical device listed. SDK 36.1 hardware qualification and Windows radio access
are not established by that inventory. No installed daemon has been changed.

Write regression coverage before implementation. Complete fixes and documentation,
freeze the batch, then execute the combined gates. A failed gate reopens the affected
item; targeted retries never replace the full denominator. Native builds, scripted
radios, private D-Bus controls, and simulator tests retain their actual evidence
level. Physical qualification must name the source identity, platform, device,
scenario, results, cleanup, and retained receipt. Missing evidence stays open.

The previous F01–F17/R1/Q1 dispositions must be retained from the complete handoff
and checked for regressions before final closure. Existing automated review threads
are leads, not a substitute for the full second-review ledger.

R2-28 implementation now includes the native minor-version/public-signature
probe, all four allowed Android request presets, owned Kotlin queue, JNI
BluetoothStatusCodes completion, Rust wire admission, backend registry and
public route. Tests were added before bridge implementation for unavailable
hosts, preset forwarding, permission failure and cancellation. The TCK now
invokes the supported operation instead of accepting only unsupported
capabilities. Rust golden generation and all edited checks remain deferred to
the frozen batch. Physical SDK 36.1 acceptance/negotiation remains unverified;
this item is not closed.

R2-29 now has native API 36 LE event handling and SDK 36.1 public snapshot
lookup, event-only generation storage, explicit snapshot ambiguity, typed
JNI/Rust/wire source failures retained for control-loss reconciliation, and
opening-snapshot fencing. New native-controlled fixtures cover API 35/36,
BR/EDR separation, link loss, old GATT callbacks and receiver cleanup retry.
Helper tests cover 36.1 snapshot values and permission errors; joined factory,
wire and provider tests cover original HCI failure delivery and reconciliation.
None has executed on the edited batch. The delayed Android broadcast versus
connection-generation correlation audit and physical qualification remain
required; this item is not closed.

R2-26 continuation (unverified source draft): Tauri now obtains the directory
identity vocabulary from the instantiated central profile, forwards independent
known/connected inventory, and preserves qualified Windows address references.
Mobile now has a separate budgeted/cancellable native ConnectedPeers request:
Android system GATT inventory and service-scoped Apple CoreBluetooth retrieval.
New Rust, Android, Apple-adapter and RN production-factory tests cover foreign
unbonded peers and zero ownership, alongside native disappearance and filter
refusals. No test/build has run against the edited batch; runtime subset auditing,
qualification, and the complete frozen-batch gates remain open.

The R2-26 mobile continuation also drafts native identifier lookup for foreign
system-connected origin references. It uses Apple exact-identifier retrieval or
Android current GATT/bond inventory, retains original deadline/cancellation and
returns null on a native miss. Tests now require an unbonded foreign reference
round trip and disappearance without creating owner-cache state. This remains
unverified source; generated bindings/goldens are deferred until batch freeze.

R2-27 tests-first continuation: a new joined public/provider/addon test file
requires acquired-write/notification sessions, actual acquisition MTU, payload
ownership before backpressure, cancellation, conflict/no-fallback behavior, HUP
terminals and idempotent teardown. At that drafting stage the source implementation was pending;
the tests had not executed. Subsequent source drafting is recorded below. Proposed public sessions own their native FD and
expose close(), with notification values using the existing bounded stream
vocabulary. No deterministic test will be labelled as physical FD qualification.

R2-27 source draft now includes a packet-FD primitive, bounded synthetic native
transport, exact BlueZ characteristic occurrence lookup, unique-owner-bound
AcquireWrite/AcquireNotify calls, flags/optional-property admission, actual MTU
validation and real/synthetic DispatchRadio forwarding. Native syscall tests
require empty packet versus HUP separation, close waking a held receiver,
oversize/failure propagation and descriptor lifetime. Central-owned session
handles, public/NAPI/provider wiring, late acquisition cleanup, lifecycle
retirement, private-bus controls and Linux Node/Bun physical tests remain pending.
No tests, formatting, generation or artifact refresh have run on these edits.

R2-27 continuation adds native central acquired-handle admission and lifetime,
a bounded pending/active registry, lease-scoped close before release, typed
database/link/adapter/source retirement, NAPI synchronous acquired write copies
and the desktop/provider/shared/public writer and notification session routes.
Each real acquisition uses a dedicated D-Bus sender. Cancelled openings close
that sender; failed FD/sender cleanup remains in a process vault keyed to the
exact parent bus/sender and is retried by canonical native shutdown. Real-FD
private-bus source controls cover delivery, HUP, deferred-reply cancellation,
missing methods and ineligible flags. All are still unexecuted. Pending work
includes private-bus conflict/owner-replacement/invalid-MTU controls, IPC routing
and declaration parity, callback/cleanup-race audits, canonical generation and
refresh, Linux Node/Bun physical FD qualification and final closure checks.
No item is closed by this source continuation.

R2-32 continuation changes the packed qualifier to include the actual installed
OS-specific public factory, its production desktop provider and identity-checked
sealed addon, with explicitly injected native synthetic radio creation. It
retains the original six deterministic-manager receipts and independent raw-waker
probe, then adds joined read/notification/cancel/deadline/late-completion/cleanup
acceptance in both CJS and ESM under Node and Bun. The Windows joined route also
reads and watches native connection parameters, so the original R2-34 omission
cannot hide behind a disconnected packed gate. CI now schedules this qualifier
on Linux, macOS and Windows with Bun 1.4.2 pinned and fail-fast disabled. The
fixture is runtime integration evidence only. These source changes and policy
regressions have not executed; R2-32 remains open until same-head packed receipts
exist on every required OS/runtime/module route.

The R2-27 producer audit additionally found acquired-notify socket send errors
were only logged while the route continued after loss. The maintained patch now
closes that route on failed/truncated delivery, producing consumer HUP, and
explicitly refuses a pending acquisition if values arrive before its FD is ready.
The complete production callback fixture adds positive delivery, failed delivery,
truncation and pending-readiness cases with exact socket/notify/watch retirement.
Current maintained patch SHA-256 is
`a5c4a43021b4aae29cc4ab552658b94997581cb547a6852e34665df6bdf7a8c3`.
These controls remain unexecuted. A Linux FD close diagnostic is reported on the
first attempt; retry observes the consumed descriptor without closing its reused
numeric fd, following Linux close(2) semantics. Dedicated-sender cleanup remains
independently owned.

R2-07 continuation adds native discovery admission identity per live lease and
exact generations. Explicit rediscovery terminalizes preceding queue positions
and acquired children; readiness helpers check their own admission before and
after probing, so a rediscovery request can end a held helper without waiting for
another readiness callback. A regression requires prompt stale-handle settlement,
zero stale writes, completed rediscovery and a successful subsequent ready write.
Cross-lease warm discovery controls and the frozen-batch verification remain
pending; no runtime closure is claimed.

### Ownership and packed-route continuation (unverified source)

Added regressions and source for per-public-connection acquired child cleanup, including shared native leases and late acquisition publication; retained failed cleanup remains retryable even after link loss. Explicit native disconnect now closes acquired descendants before a physical disconnect. Ordinary writes/subscriptions refuse conflicting acquired ownership, and acquired notify refuses a live ordinary CCCD. Apple discovery callbacks moved into a queue-confined extension included by the podspec and both Apple compile routes, preserving the 900-line gate.

Scan sightings now carry their captured native address type from BlueZ/WinRT through NAPI and the provider; missing types remain opaque. A joined provider regression changes type on the same synthetic native path to ensure no cached identity is substituted. The packed joined consumer now covers overflow/late values, stale discovery and reconnect, two logical owners, cleanup refusal/retry, adapter loss, and new native resource counters in addition to read/notify/cancel/deadline/Windows parameters. Node and release documentation describe the revised qualifier without claiming execution. No edited-batch checks, generators, platform builds or physical qualification have run. All associated entries remain open.

### Native helper source-failure continuation (unverified source)

RN atomic ready writes now race the original drain terminal, request exact native cancellation, and retain pending-operation ownership until native settlement even if cancellation is refused. New tests cover original-cause delivery, refused cancellation, no later admission after source failure and teardown before a late readiness edge. Tauri reserves native GATT admission for descriptor reads/writes, discovery and unsubscribe as well as characteristic operations before asynchronous worker execution. Tauri scan projection prefers captured address identity over a mutable lookup cache; added a current-identity regression. Strict mobile counters decoding and its deterministic fixture include the new process-owned native FIFO/acquired resource counts.

Fetched immutable b80542f3 Windows Rust job 112589503267 to `/tmp/ubm-rc21-windows-rust-b80542f3.log`. It confirms the recording fairness fixture times out at setup step 0 after 20.18 seconds, before count/loss/order assertions. This identifies the original failing phase only; it does not establish persistence latency, harmless flakiness or a product root cause. R2-01 remains open. No edited-batch tests have run.

### Acquired sender cleanup continuation (unverified source)

Acquired sender teardown now confirms `NameHasOwner = false` through the independent bound parent bus before releasing its process-vault obligation. It drops a zbus connection already marked closed instead of repeatedly invoking its shutdown after an error. Native close failures remain reported; sender observation has a two-second cleanup backstop, with the parent operation budget still authoritative, and unsuccessful/interrupted observation retains the row for retry. A private-bus controlled-state test covers an already-closed connection whose sender ACK is still owed and exact observer/vault retirement. This is source and a test definition, not execution or physical evidence.

R2-27 IPC continuation adds the acquired writer and notification routes to the
shared public IPC facade, Electron main and Tauri's real native authority. FD
handles remain scoped to the exact renderer and native connection lease. Writer
cleanup participates in admission rollback, database retirement, connection
release and renderer teardown; failed cleanup keeps the mapping and its retry
identity. Notification iterator return closes its native acquired child. Tauri
uses the native central's receive/close methods and the existing bounded stream
forwarder; source-terminal cleanup retains debt and retries. The capability no
longer declares a missing Tauri route. Public renderer and native-central Tauri
regressions cover owned bytes, returned MTU, notification delivery, refused and
repeated close, foreign-owner refusal and parent cleanup. They have not run.
The native admission audit also adds refusal while an ordinary CCCD removal is
owed, with a joined regression that retries the removal before reacquisition.

R2-16/R2-25 Tauri continuation drains events received during initial probing
before publishing the initial observation, so a delayed getter cannot overwrite
newer native evidence. Native/broadcast parameter gaps cause a live native
re-read and original getter failure ends the watch. New controlled native-central
regressions exercise the delayed probe, gap re-read and getter failure. R2-19
also carries Tauri's soft preference independently from its hard requirement;
a native-boundary fixture records both separately. None of these edits is a
passing verification receipt or physical qualification, and no finding is
closed by this continuation.

R2-25 source drafting now uses the same portable callback-answer mapper in the
real WinRT event handler and the controlled callback regressions. The native
relay explicitly publishes original getter failures and source closure. Vendor
lag discards and counts retained samples that predate reconciliation, while
preserving queued failures. A bounded registration-epoch table fences queued
callbacks from retired/replaced sources; link retirement releases its entry.
Native and desktop-provider source-health guards prevent a later watch from
silently reopening a failed source, with fresh native evidence or a new live
connection generation required for recovery. Tauri acquisition buffers initial
events in order and performs a live getter reconciliation after gaps. These
source changes and their regressions have not been executed; they do not close
the native or platform qualification obligations.

R2-22's Apple discovery draft now owns exact included-service, characteristic
and descriptor callback reservations. Cancellation and a first discovery
failure retain the native callback owner until its admitted callbacks drain;
replacement discovery is refused during that interval and cancellation cleanup
reports retryable debt. Duplicate or foreign callbacks cannot decrement another
reservation or complete the operation twice. Services-changed failure likewise
retains the old discovery callback owner, while physical disconnection retires
it. Controlled reservation regressions use native CoreBluetooth service objects.
Web graph traversal rejects service and inclusion-edge capacity excess before
publishing a database. All edited tests remain unexecuted.

R2-30 now includes controlled production-native ScanSettings.Builder regressions
for API-26 nonlegacy coded/all-supported PHY selection with batching. R2-29's
native source-failure draft additionally preserves device-address permission
failures as explicit unattributed observations instead of letting the getter
escape before the source-error boundary. These are unexecuted test definitions
and source edits, not native execution or physical qualification.

The R2-27 private-bus regression draft now covers invalid native MTUs and
replacement of the BlueZ well-known owner while an old owner's FD reply is
held. Both require the rejected FD and dedicated sender to retire, with no
old-owner publication. R2-29's reconciliation regressions require fresh
unchanged security evidence to recover an unattributed failure without
reopening its terminal watch. The production provider clears its pre-failure
deduplication cache and the deterministic wire fixture mirrors native retained
failure retirement. None of these new scenarios has executed.

The desktop CoreBluetooth R2-22 callback draft now uses a portable exact-object
reservation owner and an ordered native `DiscoveryDrained` event. Discovery
failure retains the callback owner independently of its already-settled future;
replacement discovery cannot overwrite that owner. Foreign/duplicate callbacks
are ignored before considering their error, empty graphs complete, and
characteristic parents are published before descriptor requests. Services
invalidation fails the current graph and drains its callbacks; disconnection
and adapter reset retire them. Structured mapping retains specific ownership,
stale-graph and protocol refusals. These newly drafted controls remain unrun.

Before freeze, the mandatory private-boundary changes advance mobile wire to
`ubm-mobile-wire/2` and desktop IPC to version 6 on both authoritative ends.
Tests added/updated before implementation require the preceding mobile revision
and IPC version 5 to fail negotiation before session/renderer ownership or
radio effects. Generated mobile vectors/bindings remain deferred to the frozen
batch. Historical receipts and supplied review files keep their original
version identities; no old qualification is promoted to the new wire.

R2-25's initial-watch source-health gate now also lives in native Rust and is
selected by the production NAPI/provider watch probe. This covers a source
failure that happened before the host published its connection wrapper. Tests
require no native getter on failed-source watch admission, retain the original
failure, permit an independent ordinary snapshot and recover after fresh native
observation. Joined NAPI/public tests require the watch probe flag to reach the
real addon. Binding identity seals the changed private NAPI option shape; mixed
binaries are rejected before radio admission. These controls remain unexecuted.

### Combined verification started

The full source draft was frozen for generation and combined verification. Corepack enable and workspace/Tauri Rust formatting passed. The first frozen-lockfile installation reached its prepare declaration build and failed with ten TypeScript errors across four source files; no edited-batch tests had run. The errors are being repaired together before retry. Vendor cargo formatting could not resolve the standalone manifests under the root workspace; formatting will use direct rustfmt without changing workspace membership. No item is closed by this drafting or build attempt. Retained logs: `/tmp/ubm-rc21-verification/install-frozen.log`, `format-workspace.log`, `format-tauri.log`, `format-btleplug.log`, and `format-bluez-async.log`.

### Combined verification progress: first native execution

The repaired frozen-lockfile installation, declaration build, evidence validation, plugin gate (7 suites / 67 tests), and lint/typecheck pass. Canonical UniFFI generation was copied without hand edits; its Rust gate passed 27 tests and the actual generated Python exchange passed 58 checks. Mobile golden vectors regenerated with their owning test. Backend references, API reports, private protocol declarations and dependency artifacts regenerated through their owning scripts. The standalone vendored btleplug library gate passed 60 tests on macOS. The host native-protocol gate passed both C++ tests. These are build/deterministic/exchange receipts, not physical-radio receipts.

Workspace Rust lint passed after batch compile repairs. The broader workspace test run is still in progress and has already reported a concurrent-subscription failure; its complete final result remains pending. The package test attempt stopped in prepack at the stale Apple native build-identity guard before package tests executed. Native status correctly reported all three local staged products stale. Canonical `pnpm native:refresh` is now refreshing the three products; Android ABIs completed and Apple targets are underway. Neither the prepack refusal nor partial refresh is a passing package gate.

On lx5090, the full isolated BlueZ daemon build and 193 upstream GATT tests passed. The first acquired-FD fixture compilation failed because a macro-renamed system header omitted the real `sendmsg` declaration; that fixture was repaired and the maintained patch mechanically regenerated. Current patch SHA-256 is `30c195c3c53f732bb4afa35c444d6f31dd05f57c228db6a4c6cef25a99a8e6b8`. The repeated isolated build passed the production graph, refresh, ownership/error envelope, deferred acquired-FD lifetime, protected ACK capacity, cancellation/sender-death and asynchronous cleanup refusal/retry controls. The exact compiled daemon hash is `2c3a87a12664a04f228cbeedf1ed01790cd5993a3dfee3dfe40a88687ca4b620`. It was not installed or run against a controller. Logs remain in `/tmp/ubm-rc21-verification/bluez-isolated-lx5090.log` and `bluez-isolated-lx5090-repair.log`. The original baseline denominator and physical acceptance criteria remain unchanged; no review item is closed yet.

### Complete first combined failure inventory

The first full workspace run finished with **1,180 passed, 21 failed and 3 ignored** across 88 result blocks. These are summed execution blocks (including nested child executions), not a deduplicated unique-test denominator. Its six failed targets are retained in `test-workspace.log`; exact individual failures are retained in `test-workspace-first-failures.json`. The first full package run finished with **34 failed / 396 passed suites (430 total), 82 failed / 5,329 passed tests (5,411 total)**. Exact failing suite/test evidence is retained in `test-package-first-failures.json`. These denominators are separate from the immutable baseline (420 suites / 5,287 tests). No new passing tests erase surviving failures.

Canonical native refresh completed Android, Apple and local desktop; native status passed on that frozen source draft. The Apple native protocol/Swift adapter gate passed without physical BLE. Android's first two attempts stopped on a stale Java path and missing example dependencies; after fixing the command environment and installing the example through its owning installer, compilation reached a Kotlin test fixture JVM getter collision. Tauri compilation reported six errors in a dynamic IPC object construction and newly added parameter/acquired tests. All collected failures reopen the source batch for repair; the refreshed products will be refreshed again if their identity inputs change. No item is closed and no PR is created yet.

### Repair batch following first full verification (unverified)

The complete failure set is now retained before any rerun. Repairs in progress separate the native FIFO dispatch barrier from completion ownership, preserving overlapping native operations and coalesced discovery/subscription behavior while a readiness-waiting write still holds its original place. A new native admission test requires later admission after first poll while both operation slots remain owned and invalidatable. Other drafted corrections cover protected logical-release terminal facts, explicit-null NAPI graph staging, Tauri dynamic object construction and parameter-fault controls, the Android encryption fixture getter collision, and the RN database bridge shape. Fixture repairs use real private-protocol rejection JSON, proper bounded-stream iterators/terminal records and distinct browser objects for distinct same-UUID instances. None of these edits has been retested; the batch remains open.

The subsequent repair investigation established that the atomic mobile readiness helper had incorrectly required a desktop-only core capability row. The mobile owner deliberately registers no desktop rows; its existing native readiness boundary is the truthful runtime gate and refuses Android before issuing a readiness request. The helper now uses that same boundary. Additional source repairs retain original global readiness source failures, allow peer-specific failed discovery to be reverified, and implement the newly promised RN acquired-database methods as explicit unsupported operations where mobile provides no acquired-FD mechanism. The bridge table and native-owned implementation are updated together. Verification is still deferred until the collected batch is repaired and frozen.

The first failure inventory repair batch is now frozen for a second combined pass. Native FIFO releases only its dispatch barrier on first poll while retaining completion ownership; valid same-UUID fixtures now carry distinct native occurrence identities and a separate test preserves duplicate-identity refusal. Original discovery ownership domain remains `core`; an observed open service now retains explicit `Open` metadata. Missing Android runtime-adapter source inventory and IPC revision assertions were updated without weakening old-version bootstrap refusal. Parameter/subrate TCK supported cases invoke and validate operations; noncallable descriptors remain explicitly descriptor-only (not route execution receipts). RN acquired methods have explicit unsupported admission tests on both mobile factory fixtures. All these changes remain unverified until the combined run completes.

Second combined workspace execution finished with **1,203 passed, one failed and three ignored across 88 summed execution blocks**, again not a unique-test denominator. The sole failure is pre-dispatch read cancellation losing `NotDispatched` after waiting on admission. Clippy and TypeScript pass. Generated event vocabulary passes 89 checks; regenerated wire vectors include the atomic readiness-write operation. Tauri's first executed pass reached 166/169 with three fixture-admission failures; the repaired full pass is **169/169**. Android's executed pass reached 471/472 with a mocked device omitting its mandatory native address; the repaired full Android native-protocol gate passes including merged-manifest, ELF and cache-transition checks. No physical radio was exercised. Native refresh and full package verification remain pending; findings are not closed.

The second full package run finished with **8 failed / 422 passed suites (430 total), 15 failed / 5,397 passed tests (5,412 total)**. Exact failures are retained in `test-package-second-failures.json`. The next collected repair batch makes NAPI advertisement address type explicitly nullable per sighting, preventing an unknown sighting from inheriting a previous random type; joined cases include both null and omitted input. Native queued pre-dispatch cancellation now applies the existing not-dispatched classification. Fixture repairs use iterator `return`, settle acquired native streams during close, distinguish public `BleError` from internal normalized errors, and assert stale handles after source-driven database retirement while retaining the original source error for the admitted write. Documentation signature checks preserve semantic content across owning formatter whitespace. No timeout or acceptance assertion is removed. These edits remain unverified.

The third full workspace run passes: **1,204 passed, zero failed, three ignored across 88 summed execution blocks** (not a deduplicated unique-test count). Clippy, lint/typecheck and performance (31 JS/core and five native-host measurements) pass. The isolated Linux deployment bundle also passes its owning build/producer/hash gate, with deployment binary `a4546cd9c8faedacec36a1309075f4b9f1c23ad51d5568f7621dd0ba9c23cb94`. This differs from the earlier isolated test daemon because the deployment build uses its own reviewed installation prefix/settings. The running service is unchanged. An explicit activation/restart question is pending; neither elapsed time nor bundle creation authorizes activation. Native refresh, third full package execution, joined packed execution and cross-OS/physical qualification remain pending.

The third package run reached **one failed / 429 passed suites (430 total), one failed / 5,411 passed tests (5,412 total)**. Its sole failure was a legacy scan fixture staging address type only in a separate lookup, then emitting an advertisement with no reported type. That fixture now reports random on its actual sighting; the per-sighting null/omitted-after-random regression remains intact. This test-only repair has not rerun yet. Third native refresh completed and native status passes; the same-source UniFFI exchange again passes 58 checks and the Apple native-protocol gate passes without physical BLE. The new live BlueZ plan was indexed after prepack correctly refused the unindexed document before tests.

Linux execution at the third frozen source, with complete required source inputs, finished at **1,206 passed, one failed and 68 ignored across 88 summed execution blocks**. The sole assertion still expected Linux system-connected directory capability to be unsupported; it now pins the implemented directory admission on all three desktop OSes. An unused Linux import was removed for the mandatory clippy gate. The first remote attempt stopped on an incomplete copied source tree (a compile-time TypeScript contract include); that attempt is not runtime evidence. The macOS joined packed qualifier first stopped on an incorrect one-open assumption: the actual default provider opens/closes a listing probe before opening the manager owner. Its repair tracks and verifies zero resources for both native owners. A subsequent exploratory packed attempt collided with a package build in shared output and is discarded; final pack/build verification will run sequentially. These repairs remain unverified.

The fourth Linux workspace run passes with **1,203 passed, zero failed and 68 ignored across 87 summed execution blocks**; child execution changes the aggregate block denominator, so neither result is relabeled a unique count. Linux clippy found two remaining source style errors (nested cleanup condition and a test module preceding runtime items); both are repaired without suppressions. The vendor WinRT cross-check reached its real source and found three stale watcher-test calls after option-aware configuration, plus two obsolete unused helpers. Watcher fixtures now seed and clear actual native UUID filters through the option-aware method; unused address-only construction/status-only mapping are removed. Full desktop/NAPI Windows cross-compilation stopped earlier in bundled SQLite on missing Windows C headers on this macOS host; that is not a Windows build or runtime pass. The revised packed qualifier source guard now requires both native owners' counter checks. All current cross-platform source repairs await the next combined verification.


The fifth combined Rust source pass is green on macOS and Linux: macOS **1,204 passed, zero failed and three ignored across 88 summed execution blocks**; Linux **1,207 passed, zero failed and 68 ignored across 88 summed execution blocks**. Both workspace clippy gates pass. The WinRT vendor library's Windows-target all-targets clippy gate also passes on this Mac; it is cross-compile evidence only, not actual Windows execution. Documentation generation checks, retained-evidence validation, dependency-artifact checks and the plugin gate (7 suites / 67 tests) pass. The native source batch is frozen again and canonical artifact refresh is underway. Package and packed public-route execution will run sequentially after refresh to avoid shared build-output interference. Live BlueZ activation/restart and physical Windows access remain unanswered prerequisites; no daemon was changed and no review item is closed by these partial receipts. Logs are retained under `/tmp/ubm-rc21-verification/` with the `fifth` suffix.


Canonical fifth native refresh completed Android, Apple and local desktop products; native status passes on the frozen source. The sequential full package gate passes **430 suites / 5,412 tests, zero failures** (`test-package-fifth.log`), preserving this denominator separately from the original baseline. The packed macOS public Node/Bun qualifier is now running after package completion. This passing local package result does not close the cross-platform CI, Windows recording setup or live qualification obligations.


The fifth packed macOS run reached the real installed public route but failed its stale-handle assertion after discovering through a different logical lease; switching that second lease to explicit rediscovery reproduced the same assertion. The current semantics give logical leases independent generation validity and cleanup, and the core invalidates the refreshing connection's snapshot. The fixture now explicitly rediscoveries through the owner of the old handle, retains the required `gatt.stale-handle` rejection and then obtains the surviving lease's own database. This fixture correction is under rerun, with both failed logs retained; no production behavior was changed and no packed qualification is yet claimed. Reference typechecks and private-protocol generation checks also pass.


The next joined macOS packed run passed owner-specific stale-handle and shared cleanup assertions, then reported `operation.disconnected` during adapter loss. Waiting for nonzero native GATT admission reproduced the same public result. The current normative event vocabulary requires `operation.reset` for adapter loss during pending work; `adapter.powered-off` is the already-off admission refusal, so the qualifier's initial specific-code expectation was also corrected. The core teardown now selects its existing reset outcome for `adapter-loss`, preserving quarantine ownership and uncertain writes. A new regression covers dispatched and queued reset, no queued native dispatch, late acknowledgment and zero final counters. A more extensive tentative snapshot/error override was discarded before verification; no public contract or native ABI was changed. The collected source/fixture repair batch is frozen for full package execution followed by sequential packed rerun. No failed run is relabeled a pass.


The sixth full package run reports **one failed / 429 passed suites; three failed / 5,410 passed tests (430 / 5,413)**. All three are in the coordinator fixture: the new ownership assertion used a zero-byte payload (so resource ledger zero did not measure the separately retained quarantine), and two existing scoped imports were accidentally removed while discarding the larger tentative source draft. The fixture now retains four bytes and asserts the actual pending native drain; both original imports are restored. Lint/typecheck passed the single-line production reset correction. The seventh full package rerun is underway, before the next sequential packed execution.


The seventh sequential combined pass is green: full package **430 suites / 5,413 tests, zero failures**; native status passes; the macOS packed public qualifier passes all four actual installed executions (Node CJS, Node ESM, Bun CJS, Bun ESM). Each traverses the public host factory, default provider and sealed real NAPI dispatch with an explicitly synthetic native radio, including deadline/cancel/late completion, native stream overflow, owner-specific rediscovery, independent logical leases, failed cleanup retry, native-admitted adapter reset and zero final native counters for both listing and manager owners. This is joined runtime integration evidence, not physical CoreBluetooth radio evidence. Logs: `test-package-seventh-joined-contract.log`, `native-status-seventh-joined-contract.log`, `bun-packed-macos-seventh-joined-contract.log`. The broader packed install smoke is underway. Linux/Windows joined packed execution, same-head CI and live qualification remain pending.


The generic local packed install smoke stopped before consumer tests: archive validation correctly rejected partial desktop prebuild staging (the four Linux/Windows targets are absent on this Mac). The failing log is `pack-install-smoke-seventh.log`; it is not a passing smoke or a product runtime failure. Complete same-head prebuild assembly and packed qualification remain required. A source checkpoint will permit the required clean Linux preflight (whose clean checkout has no gitignored partial desktop staging), followed by CI and the explicitly non-publishing matrix workflow. Publication remains outside this task.


The clean Linux preflight at checkpoint `6e1ad76f76acc1c495687bcb00c22e336c54f22c` finished in 252 seconds: package failed in the dedicated private-bus gate (47 / 48 pass), Tauri passed, Android stopped before compilation under the host's default JDK 25. The exact acquired-FD failure was `capability.unsupported` instead of `backend.reset` after a captured daemon owner replacement. Full workspace runs had ignored these dedicated-session cases; their green aggregates did not cover this gate. The source repair separates bound-authority admission from captured-owner observation, rejects replacement or disappearance before FD publication, preserves structured owner/D-Bus evidence and leaves original admission/discovery refusals unchanged. A new absence control also requires sender death, descriptor HUP and zero retained acquisition debt. JDK 17 is already installed and will be selected explicitly for the complete preflight rerun. Current repair batch is unverified; logs remain in `preflight-linux-6e1ad76f.log` and the retained Linux preflight cache. No push, live service change or PR occurred.


The owner-epoch repair batch passes the dedicated private-bus gate: **85 tests across four suites (1 + 49 + 15 + 20), zero failures**, followed by successful isolated daemon-extension build (`built-not-radio-qualified`). Linux workspace clippy passes. Canonical refresh rebuilt Android, Apple and local desktop; native status passes. The eighth sequential package pass is **430 suites / 5,413 tests, zero failures** and the macOS joined packed Node/Bun CJS/ESM qualifier passes with the new sealed identity. NAPI source digest is `a0986625ffc77b6eb25b5165b3410b1bbd43517be012ca49cc73567b6db1e61c`; binding schema is unchanged. Logs carry the `eighth-owner-epoch` suffix. The next clean source checkpoint will rerun complete Linux preflight with installed JDK 17 selected; full six-target artifact assembly, same-head cross-platform CI and live qualification remain pending.
