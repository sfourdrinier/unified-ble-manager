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
