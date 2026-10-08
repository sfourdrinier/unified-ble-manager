# Round-three remediation batch

Current implementation record for the 17 confirmed findings in
[VERIFICATION.md](VERIFICATION.md). The original review and reproduction
receipts remain unchanged. Implementation is on `release/5.0.0-rc.21` for
existing PR #251; no separate remediation PR is intended.

## Acceptance mapping

| Findings       | Corrective coverage                                                                                                                                                                       |
| -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R3-01/02/03    | Authenticated Electron concurrency, operation-owned rollback and retry, database/watch admission failures, actual Electron/Tauri priority factory routes, late accepted/rejected outcomes |
| R3-04          | Exact retained native device owner, failed retirement and same-owner retry, provider terminal cleanup, replacement-generation guard                                                       |
| R3-05          | Controlled shutdown admission barrier, owned FIFO drain and foreign-consumer denial                                                                                                       |
| R3-06/07/09/17 | Opening and established reconciliation fences, queued newer values/errors, transient recovery, retained original failures and source closure                                              |
| R3-08          | Explicit native/provider security reprobe, healthy recovery and newer-fault preservation                                                                                                  |
| R3-10          | Mechanically extracted production Pair/lease functions: Classic accepted/completed/refused/sender-death requests, overlapping LE release and LE positive controls                         |
| R3-11          | Exact prefix parsing, high native handles and distinct discovery/acquired-route identities                                                                                                |
| R3-12          | Native callback failures, exact attribute epochs, accepted FIFO before original terminal, failed cleanup retry and retained fault evidence under independent-attribute queue overrun      |
| R3-13/14/15    | Android advertised versus cached name, capture-clock provenance through JNI/wire/public projection, raw byte ownership and byte quotas                                                    |
| R3-16          | Native direction flags/runtime admission, exact lease, real NAPI/provider/public and Tauri/IPC route, packed Windows Node/Bun route, read-only capability                                 |

## Verification status

The corrective source and regressions have completed the focused checks below.
These are working-tree receipts retained under
`/tmp/ubm-rc21-verification/r3-frozen-batch`; the directory name does not imply
a qualified frozen commit. Keep item-by-item closure in the existing
[rc.21 remediation ledger](../RC21_REMEDIATION.md), and retain receipt bytes
with their actual source/artifact identities before declaring acceptance.

| Check                                                                    | Actual result                                                                                                                    | Retained receipt                                                           |
| ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| Vendored native mechanisms                                               | 69 tests passed                                                                                                                  | `vendor-tests.log`                                                         |
| Rust desktop library                                                     | 367 tests passed after correcting two unit fixtures                                                                              | `desktop-lib-retest.log`; original failures remain in `rust-tests.log`     |
| Other Rust workspace targets                                             | Passed in the original workspace run; that run's overall result failed only the desktop library target, subsequently rerun above | `rust-tests.log`                                                           |
| Mobile wire, raw scan and capture provenance                             | 260 tests passed; a separate 108-test scan batch/clock-scope check passed                                                        | `mobile-scoped-retest-confirmed.log`, `mobile-batch-clockscope-retest.log` |
| Electron admission, actual IPC factories and desktop notification faults | 91 Electron/priority tests passed; 30 desktop diagnostic tests passed on corrected rerun                                         | `electron-focused-jest.log`, `electron-notification-focused-jest.log`      |
| TCK/PHY registration and observation                                     | 30 tests passed                                                                                                                  | `tck-phy-correction-final.log`                                             |
| Shared native cleanup ownership                                          | 12 tests passed                                                                                                                  | `shared-release-cleanup-retest.log`                                        |
| Public projection and scan validation                                    | 100 tests passed                                                                                                                 | `public-projection-final.log`                                              |
| Cross-copy public declarations                                           | Two tests passed                                                                                                                 | `cross-copy-retest.log`                                                    |
| Exact production Pair/bearer controls                                    | Seven scenarios passed using 48 extracted production functions                                                                   | `pair-bearer-controls-rerun.log`                                           |
| Documentation gate                                                       | Passed, including generated API reports, HTML, index and required formatting                                                     | `docs-process-final.log`                                                   |

These check sets overlap; their counts must not be summed into a new benchmark
denominator. The first complete package run had **424 passed / 10 failed suites**
and **5,476 passed / 24 failed tests** (`package.log`). Its identified failures
have passing scoped corrective retests; no fresh complete package pass is
claimed yet. The Electron receipt likewise retains its first two diagnostic
expectation failures, followed by the passing diagnostic-only rerun.

Native artifact refresh completed for Android, Apple and macOS desktop; all
three source/build identities pass `native:status` (`native-refresh-final.log`,
`native-status-final.log`). Tauri passed 174 tests and warning-free clippy
(`tauri-tests-final.log`, `tauri-clippy-final.log`). Rust all-target clippy,
TypeScript checking and focused ESLint pass. Plugin tests passed 67 tests and
evidence/dependency-artifact validation passed. The vocabulary generator and
its 89-test suite passed (`vocabulary-regeneration.log`). An independent
read-only agent review found no concrete blocker in the final cleanup,
timestamp, scan-validator, PHY TCK or process-document changes; this is source
review evidence rather than CI or hardware execution.

The immutable candidate, clean final cross-platform CI, required packed-consumer
gates and affected physical qualification remain pending. Record their actual results and identities when complete;
neither implemented code nor a previous head's passing job closes those gates.

Compilation, synthetic-radio public routes and exact-function controls do not
establish physical-radio qualification. Runtime changes require only affected
physical scenarios to be repeated. Retain the earlier platform/hardware gaps
and failures; this batch does not promote an evidence label or establish GA
qualification.

## Post-freeze Windows compile correction (R3-04/R3-12)

Windows CI at `27060076` rejected an unused disconnect classifier compiled on
Windows after its caller moved to retained native-owner cleanup. It also
reported the CoreBluetooth notification publisher as unused on Windows.
The correction aligns both helpers with their production callers and preserves
`cfg(test)` coverage; no warning is suppressed and no radio behavior changes.
An independent OS-conditional caller audit found no further defect in this
class. Windows-target vendored-library Clippy and macOS all-target workspace
Clippy pass. The disconnect classifier regression and all six notification
queue/fault-lifetime controls pass. Canonical Android, Apple and macOS native
artifacts were refreshed for the new source identities. Cross-compiling the desktop crate on macOS is blocked by the
missing Windows C runtime headers needed by bundled SQLite; this is not a
Windows desktop compile pass. The new candidate must pass Windows CI and the
mandatory clean preflight; unchanged hardware evidence remains applicable at
its original identity and scope.

## Capture-order follow-up (R2-12/R2-13, R3-14)

Automated review 4213808529 identified a surviving invariant violation: receipt
order must not overwrite or combine facts from the future of a packet when its
capture clock is comparable. The reproduction failed through both RN Android
and authenticated Electron public scan filtering before the correction. The
shared cache now retains per-fact source origin, explicit clock scope and ingress
ordinal, orders comparable facts by capture time, and uses receipt order for
incomparable clocks. Projections use only fresh selected facts at or before the
current packet; fully matching raw packets retain their original observation.
Duplicate captures do not renew freshness. Expired payloads are released while
ordering metadata remains bounded by the existing fact limit and scan lifetime.

Cross-platform assessment: RN Android and rich Electron preserve source metadata
and exercise capture ordering. Desktop observations without capture clocks and
compact Electron/Tauri IPC retain receipt ordering; no transport metadata or
platform clock is fabricated. Web and shared public paths use the same cache
contract when applicable. Native ingress, private wire formats and native artifact
identities are unchanged.

Regression evidence: 37 shared-cache tests pass, including keyed/scalar replacement,
future-fact exclusion, duplicate expiry, clock scope/origin changes, equal-capture
ordinal ordering, bounded watermark payload release, truthful raw packets and
compact IPC receipt ties. Rejected list facts project explicit derived absence;
genuinely observed empty lists remain unchanged. Two RN/Electron public-route suites pass 91 tests,
including four new capture-order regressions and Electron metadata cloning. These
counts overlap existing coverage and are not a new full-suite denominator.
Receipts: `scan-source-order-test-final.log` and
`scan-capture-public-corrected-pass.log` under the retained batch directory.
TypeScript checking, focused ESLint, the package build and documentation checks
pass. Independent read-only review found zero remaining concrete source blockers
after correcting rejected-list absence; this is not execution evidence.
This is deterministic public-route evidence; final clean preflight and
cross-platform qualification must identify the new frozen fixing commit, and no
physical-radio claim or evidence-label promotion is made.

### Clean-checkout compact projection correction

Clean Linux preflight at `3968353e` exposed one regression: 433 package suites
passed and one failed (5,532 passed / one failed test), while Tauri passed.
The cache recreated genuine empty compact lists, causing a spurious merged
projection; adding absent connectability then violated the existing strict public
shape. The correction preserves unchanged list identities and optional absence.
A genuine merge that borrows connectability into a minimal packet reports current
unreported TX power as null rather than borrowing an old measurement. Private
validators remain strict. Minimal and mixed full/minimal compact projections are
covered through direct normalization and the real public filter. The original
quota-terminal regression remains unchanged and passes. The two complete
cache/public-query suites pass all 140 tests with no skips; TypeScript checking,
focused lint, package build and documentation checks pass. Receipt:
`scan-compact-projection-final.log` under the retained batch directory. Independent narrow
source review found zero remaining blockers. The failed freeze is retained as
failed evidence; the corrective successor requires a new clean preflight and
current-head cross-platform qualification.

## Effectful-control outcome follow-up (R2-28, R3-03)

Automated review 4213908630 exposed incorrect ownership and retry-safety
classification for connection-control effects. Shared-core priority, subrate and
PHY requests used noncommitting cancellation semantics; MTU already declared
effects. The mobile owner also omitted all four requests from commit-envelope
classification and the actual foreign-radio dispatch marker. The paired strict
TypeScript wire decoder likewise treated commit metadata as write-only. The
ordinary RN factory uses the direct native-owner route, so the shared-core
coordinator defect is not attributed to every RN call.

The correction treats these requests as effectful end to end. Existing native
dispatch tracking distinguishes pre-dispatch refusal from an uncertain effect
after radio handoff. Owner-generated pre-dispatch cancellation and observation-only
timeout remain noncommitting; explicit platform retry outcomes are preserved.
The strict decoder and deterministic fixture use the matching effectful operation
set without weakening validation. A complete response already validated at the
core boundary retains its accepted/rejected result while physical retirement
drains. That retirement remains part of release/destroy ownership, keeps the
queue occupied, wakes drain waiters at completion, and uses the existing finite
cleanup drain bound. No microtask grace or unbounded cancellation-acknowledgement
wait was introduced. Pending requests still settle promptly with uncertain
commitment and retryability `never`; unvalidated native callback arrival is not
a successful terminal contender.

Cross-platform assessment covers shared core, mobile Rust owner, foreign radio,
strict wire ingress and RN public projection. Android implements the affected
requests where runtime capability admits them; Apple genuine unsupported controls
remain unsupported. Desktop priority already classifies native request effects,
and supported Electron/Tauri routes preserve its settled owner response. Read-only
RSSI, PHY, parameters and effective-MTU observations remain noncommitting. Web
unsupported controls gain no fabricated mechanism. Protocol vocabulary and native
ABI are unchanged. Canonical Android/Apple artifacts require source-identity refresh;
desktop native sources are unchanged.

Regression evidence: 340 tests across five complete core/coordinator/public/wire
suites pass, including all four requests, accepted/rejected validated answers,
abort/deadline, pending cancellation, pre-dispatch/queued refusal, physical
retirement during destroy and public error projection. Native session verification
passes 90 tests with actual session/foreign-radio dispatch boundaries, including
post-dispatch cancellation/deadline, pre-dispatch cancellation, accepted/rejected
answers and a read-only PHY control; focused native Clippy passes. These are
controlled-boundary tests and not physical-radio proof. Receipts:
`effectful-controls-wire-final-verified.log`,
`mobile-effectful-controls-full-session.log` and their retained handoffs. Counts
overlap existing coverage and do not replace the frozen full-suite denominator.
Original diagnostic failures, incomplete baselines and fixture corrections remain
retained with their actual limitations.

Final type checking, focused ESLint, package build, documentation and dependency
artifact checks pass. Independent source review found zero remaining blockers after
the typed drain query correction. Canonical Android and Apple refresh completed;
`native:status` reports all applicable groups fresh and desktop was unchanged.
Android source digest: `ad62d6771fab71d343022e340d99b1e018669fbcc21ab9b091e1fafdd91f6d45`;
Apple source digest: `e9ff2f9035f926fca326abe4efe482b0886ce9b0f3216fea690652b11e94b13c`.
An earlier refresh refused a source identity changed during its build; that failed
receipt is retained and is not a qualified artifact. Final refresh receipts:
`effectful-native-refresh.log` and `effectful-native-status.log`.

The new fixing candidate must be pinned, then pass clean preflight and current-head
cross-platform qualification.
The previous scan candidate's green Linux run does not qualify this new batch.
Existing physical qualification gaps remain explicit; no evidence label is promoted.

### Generated mobile wire-vector correction

Clean preflight at `4aaaf030` failed two assertions in the same golden-vector
suite: 434 suites / 5,578 tests passed, one suite / two tests failed; Tauri
passed. The retained subrate refusal vector still carried null commit metadata.
The existing generator already invokes the real Rust session owner, so its source
required no change. Running its documented command
`UBM_MOBILE_GOLDEN_WRITE=1 cargo test -p ubm-mobile --test golden` regenerated
the vectors successfully. The sole artifact change is subrate refusal commit
`null` to `not-dispatched`; its owner retryability stays `never`. All effectful
control vectors were checked. Three complete golden/wire/subrate suites pass
322 tests. Receipts: `effect-controls-golden-regenerate.log` and
`effect-controls-golden-replay-final.log`. The golden artifact is outside native
source-identity inputs, so no native rebuild is required. The failed candidate
is not qualified for push; its corrected successor requires clean preflight and
current-head final qualification.

### Preserve explicit backend refusal outcomes

A final related-path check found that shared coordinator failure handling replaced
an owner's explicit `not-dispatched` commitment and retry advice with uncertainty
for effectful requests. The real desktop priority route supplies that metadata.
The correction preserves explicit owner commitment, retryability and native
details; uncertainty is only the fallback when an effectful owner omits commitment.
A typed owner failure with explicit commitment already validated at the core
boundary also retains its answer during physical retirement. Unstructured failures
and failures without commitment metadata remain prompt cancellation contenders.
The same existing finite drain and queue ownership apply to known failure and
success, and waiters wake only after retirement.

The final six complete focused suites pass 448 tests, including 73 current core
controls cases. Type checking, focused ESLint, package build and documentation
checks pass; independent narrow source review found zero blockers. Receipts:
`effect-controls-final-integration.log`, `effect-controls-final-package-build.log`
and `effect-controls-final-docs.log`. Native source and artifact identities are
unchanged. The `974606fa` preflight was deliberately interrupted when this
related source correction invalidated its freeze; 435 suites / 5,580 tests and
67 plugin tests had passed before interruption. Its incomplete receipt is not a
complete preflight pass or a product-test failure. The new immutable successor
requires clean preflight and current-head qualification.
