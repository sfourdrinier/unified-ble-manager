# Changelog

All notable changes to `unified-ble-manager` are documented here.

## [5.0.0-rc.11] - 2026-09-25 (prerelease)

This candidate addresses the three P2 findings in the rc.10 review. It does
not promote any backend support label or claim physical-radio qualification.

### Lifecycle and consistency

- An automatic scan-stop failure remains available as a diagnostic without
  permanently contaminating later cleanup. Once native release and local
  iterator cleanup succeed, the public scan registration retires and repeated
  stop resolves; outstanding native or local cleanup failures remain visible.
- Mobile scan admission carries its originating operation's radio-request
  tracker into the spawned task. Session queued/dispatched and pending-request
  diagnostics now account for the native scan request while preserving prompt
  caller cancellation and continued orphan cleanup ownership.
- IPC `withDiscoveredConnection` gives connect and discovery one acquisition
  deadline, matching the in-process helper. Discovery receives only the
  remaining budget; cancellation and exactly-once connection release remain
  intact.

### Release reliability

- The tag workflow keeps a single bounded npm tarball-visibility retry budget
  and extends it from 10 to 20 minutes. Invalid or not-yet-visible registry
  metadata is retried without repeated stack traces. The rc.10 publish command
  succeeded, but its 118 MB tarball became visible after the previous window;
  the recovery run bound that registry artifact without republishing.

## [5.0.0-rc.10] - 2026-09-25 (prerelease)

This candidate addresses the three release-relevant findings in the rc.9
review and qualifies the additional mobile cancellation scenario. It does not
promote any backend support label or claim physical-radio qualification.

### Lifecycle and cleanup

- Confirmed parent release settles in-flight public scan stops and closes scan
  state streams on both in-process and IPC managers. Failed local iterator
  cleanup remains visible and retryable; a failed parent release retains child
  ownership. A failed GATT acquisition waiting in provisional compensation
  rejects with its original error once the parent authoritatively releases.
- IPC cleanup retries remain single-flight across bounded destroy attempts,
  including raw malformed-plan compensation. Late completion cannot recreate
  lease-owned debt after the parent has released.
- Mobile scan admission remains cancellation-aware while a prior cleanup-only
  physical scan stop is held. The host continues that cleanup and fences fresh
  native starts; the caller does not wait behind the unrelated cleanup ticket.

### Stream accounting

- IPC reports saved upstream loss even when only its pending overflow control
  was displaced. The notice preserves the upstream policy and cumulative
  counters without adding the same loss twice; local-only pending loss keeps
  its `drop-oldest` policy.

### Release qualification

- The mobile golden-wire fixture now attaches its Apple restoration listener
  before injecting a live restoration callback. Pre-session restoration remains
  covered through the durable claim path; the two golden assertions share one
  scripted-radio run.

## [5.0.0-rc.9] - 2026-09-24 (prerelease)

This candidate addresses all six findings in the public rc.8 review. It does
not promote a backend support label or claim physical-radio qualification.

### Lifecycle and cleanup

- Mobile shared-scan admission no longer mistakes a cleanup-only physical scan
  for a live source. An immediate replacement confirms old-generation cleanup
  before a fresh native start; tests require an actual advertisement and prove
  refused cleanup remains owned.
- Manager destruction reaches authoritative parent release within a bounded
  child-drain budget even when scan stop or subscription cleanup never settles.
  Late child failures remain observable without turning a confirmed parent
  release into a false cleanup failure.
- Public GATT subscription retry uses the caller's original deadline and abort
  budget. Failed provisional cleanup blocks only a conflicting GATT path, not
  subscriptions on unrelated devices; manager teardown still owns all debt.
- The IPC public scan adapter preserves the missing-plan protocol error and
  any compensating stop failure, including rejected aggregate errors, until
  cleanup or authoritative lease release is confirmed.

### Stream accounting

- IPC preserves terminal-only and evicted upstream loss counters, combines
  cumulative upstream loss with independent local pending-buffer loss, and
  excludes control-record bytes from lost-value arithmetic. Regression tests
  cover dropped items and bytes, replacements, and unknown aggregate
  attribution.

## [5.0.0-rc.8] - 2026-09-24 (prerelease)

This candidate addresses the seven findings in the public rc.7 review. It
does not promote a backend support label or claim physical-radio qualification.

### Ownership and recovery

- Desktop scan-start compensation reconciles successful and failed early stops
  with the authoritative native start result. A cancelled or refused start no
  longer leaves phantom scan ownership; Rust and TypeScript lifecycle tables
  agree on the `stopping` to `failed` transition.
- Mobile shared-scan admission transfers central-retained cleanup after an
  error to its existing process-owned retry driver. A replacement session can
  scan on the same live host before shutdown.
- GATT subscription acquisition is compensated if public result construction
  fails. Failed removal stays with a manager-scoped retry owner, while a
  confirmed whole-manager release settles that debt.

### Delivery and consumer guidance

- The shared IPC layer reports aggregate event loss with unknown child-stream
  attribution, preserves terminal and malformed-event diagnostics, and keeps
  buffered values ahead of an early terminal within its bounded control budget.
- A historical event-pump failure no longer makes `destroy()` fail repeatedly
  after the underlying lease is confirmed released; genuine cleanup refusals
  remain retryable.
- Terminal connection-event and GATT notification cleanup now feeds refused
  unsubscribe attempts into the manager's lease-owned retry ledger.
- The executable Tauri guide checks resolved cleanup receipts, including
  `release-failed`, and preserves simultaneous operation and cleanup failures.

## [5.0.0-rc.7] - 2026-09-23 (prerelease)

This candidate addresses the thirteen findings in the public rc.6 review.
It does not promote any backend support or physical-radio evidence label.
The truthful React Native diagnostic and limitation wording from finding 159
remains: the active route is `ubm-mobile-wire/1`.

### Runtime and ownership

- Mobile Rust session disposal now closes admission and waits for admitted
  operations without holding the operation gate. Concurrent disposal shares an
  attempt result; a later call can retry a failed attempt.
- Desktop and mobile scan-start compensation retain native stop ownership after
  a refusal or timeout. The first mobile scanner's zero-member orphan has a
  process-owned retry path; an 18-case cancellation/deadline/cleanup matrix
  covers zero, one, and two prior members.
- Android background-scope teardown and Apple session teardown retain failed
  cleanup in process-owned retry owners, rather than only logging the failure.
- GATT subscribe results report delivery observed by the native operation,
  instead of inferring it from the request. The observed result is carried
  through the mobile, desktop, Electron, WinRT, and Tauri bridges.
- Tauri and Web reject unsupported background manager options explicitly.
  Numeric Bluetooth UUID inputs accept valid zero-prefixed short forms.
- Electron's public transport propagates aggregate overflow, terminal, and
  iterator failures to active streams, with cleanup remaining retryable.
- A Tauri notification poll already in flight cannot overtake a requested
  disconnect and misreport its stream terminal as `connection-lost`.

### Distribution and consumer proofs

- Android's default prebuilt ELF verifier runs within Gradle's JVM and no
  longer requires a shell; malformed and no-shell regression fixtures cover it.
- The packed Tauri gate builds a linked external application and binds its
  receipt to the exact npm tarball later published by the release workflow.
- Tauri's CLI scaffold and guide share one install recipe using a local plugin
  path and consuming-root Cargo patches, not an unpublished crates.io plugin.
- The Tauri frontend guide reads the actual nested scan observation and owns
  manager, scan, and connection cleanup. Its exact code block is typechecked
  and executed under failure injection.
- A canonical 5.x SVG mark and reusable raster app icons replace the inherited
  BLE PLX artwork in the Tauri proof app, Web example, and packed-consumer
  fixture.

## [5.0.0-rc.6] - 2026-09-23 (prerelease)

The immutable `v5.0.0-rc.5` tag stopped before npm publication. Its new packed
external Tauri consumer proof compiled a real Linux Tauri application after
the release-only Android builds, but the canonical publish job installed only
the NAPI/D-Bus build prerequisites. It therefore lacked the GTK/WebKit system
libraries required by Tauri and failed at `glib-sys` before the immutable npm
tarball was created.

This candidate installs the same complete Tauri Linux prerequisite set in the
publish job that already gates the CI package lane. A regression test checks
both workflows so their packed-consumer environments cannot drift apart again.
It otherwise carries the complete rc.5 review remediation unchanged; neither
candidate promotes a backend evidence label or claims new physical-radio
qualification.

The prior truthful React Native diagnostic and limitation wording departure
also remains part of this candidate (finding 159): the active transport is
`ubm-mobile-wire/1`, not the obsolete JSI control boundary.

## [5.0.0-rc.5] - 2026-09-23 (prerelease)

This candidate closes the twelve findings from the public source and
documentation review of `v5.0.0-rc.4`, and confirms the review's additional
shared-scan scenario with deterministic Rust reproductions. These changes do
not promote any backend evidence label; physical-radio qualification remains
separate from package SemVer.

### Fixed

- React scan-hook cleanup can run again after an earlier cleanup completed, so
  a scan admitted after unmount is stopped instead of inheriting a stale
  promise. Terminal scan failures now preserve their structured public error;
  normal closure, owner release, cancellation, and deadline expiry still end as
  normal stopped outcomes.
- `GattCharacteristic.writeWhenReady()` invokes the source method with its
  original receiver, preserving instance-backed implementations.
- Android React-context invalidation now shares one admission barrier with
  queued session opens. Work accepted before invalidation settles without
  creating an unowned native session afterwards.
- Android invalidation transfers a refused session disposal to a process-owned
  retry owner. The disappearing React module no longer forgets cleanup debt,
  and retries retain the session until Rust confirms release.
- The active Android Gradle manifest now declares companion-presence
  observation permission. Its test resolves the manifest selected by Gradle,
  and a fresh consumer module checks the merged output.
- Expo `neverForLocation: false` writes a manifest-merger removal marker instead
  of merely omitting the inherited library attribute. Merged-manifest tests pin
  `true -> false -> true`, and the API 30 fixture declares the location
  permission requested by the documented `legacyLocation: "auto"` path.
- A caller deadline expiring after a shared-scan widening start can no longer
  strand existing members. Successful compensation terminates displaced
  members; refused compensation retains the replacement operation and retries
  it through the existing member's ordinary stop.
- Tauri routes retain the attachment identity validated from their envelope
  through execution. A concurrent adapter-reset rebind can no longer let a
  request from the ended attachment execute against the new generation.

### Documentation and consumer proofs

- Candidate install commands are pinned to `unified-ble-manager@5.0.0-rc.5`
  and distinguish npm `next` from the stable `latest` line.
- The external Tauri recipe now places the mandatory `btleplug` and
  `bluez-async` patches in the consuming Cargo root. CI packs npm, installs it
  outside this workspace, and compiles a fresh Cargo consumer from that layout.
- The Tauri Battery example resolves canonical UUIDs through
  `gatt.characteristic('180f', '2a19')` and proves that the read actually runs.
- The Expo Android 11 recipe, generated manifest, and runtime permission request
  now agree on `ACCESS_FINE_LOCATION`.
- Contributor guidance names `UnifiedBleRustCore` and `ubm-mobile-wire/1` as
  the active React Native factory route; `UnifiedBleProtocolControl` is labeled
  as historical compatibility surface.
- The prior truthful React Native diagnostic and limitation wording departure
  remains part of this candidate (finding 159): the active transport is
  `ubm-mobile-wire/1`, not the obsolete JSI control boundary.

## [5.0.0-rc.4] - 2026-09-23 (prerelease)

The immutable `v5.0.0-rc.3` tag stopped before npm publication because its
release-only clean-tarball acceptance created a temporary consumer without a
`packageManager` pin. Corepack selected pnpm 12.5.1 instead of the repository's
pnpm 10.14.0, then the strict offline install could not resolve
`@babel/runtime@^7.29.7` from that empty pnpm-v11 cache. `5.0.0-rc.4` pins the
temporary consumer to the repository package manager and primes its offline
dependency metadata before the acceptance install. It keeps every preceding
failed tag immutable and requires npm registry verification before this release
is described as published.

The immutable `v5.0.0-rc.2` tag stopped before npm publication while the
release-only Android source build required pinned-Rust Android targets that the
publish workflow had not installed. The workflow now installs
`aarch64-linux-android` and `x86_64-linux-android` for the pinned toolchain
before either Android Gradle build. The package, native plugin, generated
references, examples, and current release guidance now identify
`5.0.0-rc.4`; the 5.0 behavior and qualification notes below apply to this
candidate.

The immutable `v5.0.0-rc.1` tag stopped before npm publication because
`android-actions/setup-android` tried to install Google's removed legacy
`tools` SDK package. The release workflow now explicitly installs only
`platform-tools`, matching both Android CI build jobs. A regression guard
checks all three setup steps.

The first 5.0 release-candidate tag, `v5.0.0-rc.0`, stopped before npm
publication: the Linux native prebuild passed but its Electron smoke needed a
display server on GitHub's headless runner. This candidate runs that smoke
under Xvfb, preserving the Electron ABI check.

How to read this section: each entry states a behavior change in plain
words first, then the exact mechanism. A `finding NNN` reference is the
independent review finding it answers; `issue #NNN` is the tracked issue.
"Departure from 4.x" marks behavior that intentionally differs from the
4.x line; "Breaking" marks a change that requires consumers to update
their code. Physical-device anecdotes below (a Samsung phone, Polar H10
straps) motivate individual fixes; they are rationale, not support claims.
Backend support labels come only from retained evidence — see
`docs/generated/PLATFORM_SUPPORT.md`.

This prerelease carries the Rust-first distributable: the npm artifact ships
the Rust workspace sources (`crates/`, `bindings/`) and committed Android native
prebuilds (`arm64-v8a`, `x86_64`), so packed consumers build and load the shared
core. The N-API `UbmCentral` dispatch, Tauri plugin, React Native shared-core
seam, 5.x podspec/JNI drain, full scan-discovery facts, and sealed native build
fingerprint are included. The remaining cutover follow-ups are tracked in the
5.0.0 gate ledger and `docs/5.0.0-PACKAGING.md`.

### Changed

- **Background continuation: an app can say what a wake should do.** A new
  declared option, `background.continuation`, is the standing order the library
  executes when the operating system wakes a terminated app because a known peer
  reappeared. `record-only` (the default, and today's behaviour) notes the peer
  and stops. `native` reconnects and resubscribes through the Rust core without
  JavaScript, so values arrive while the app is still closed and drain, with the
  loss accounted, when a session next opens. `headless-task` and
  `foreground-service` are declared and report `capability.unsupported` with the
  reason "not implemented in this release" — never "the platform cannot" — and
  arrive in a later release, as does Apple parity for the declared policy.
  Reconnect policy stays the application's: the app declares the order while it
  is alive, and the wake executes only what was declared. Four capabilities
  report what a host can do (`background:wake-on-appearance`,
  `background:native-resubscribe`, `background:headless-task`,
  `background:wake-notification`); desktop hosts scope out of them entirely.
  Obtaining any platform prerequisite (battery exemption, notification
  permission, foreground-service type) is the consuming application's business:
  UBM implements the mechanism and reports the platform's refusal verbatim.
- **Companion associations no longer accumulate (Android).** `associate()` finds
  an existing association for the device and reports `already-associated` rather
  than silently creating a second one, the presence wake is idempotent per
  device, and applications can now list and remove associations. Two
  associations for one strap had the operating system delivering the wake twice
  per appearance; a live run of 40 appearances now delivers exactly one each.

**Connection supervision and one vocabulary for every host.**

- **The connection supervisor honours terminal retryability.** A
  `connection.failed` (or any listed code) reporting `retryability: "never"` —
  an insufficient-authentication refusal, or a peer that removed its pairing —
  stops the supervisor after one attempt instead of retrying with backoff.
  `caller-decides` keeps the existing retry behaviour. The staged TCK
  arbitration program, which still pinned the pre-join refusal, now pins the
  join.

- **Same-peer connects join on every backend.** A second connect to a
  connected peer leases the peer's link with an independent generation
  instead of failing `connection.already-owned`, on every backend — WinRT,
  CoreBluetooth, Web and React Native/Android now join the same way the
  desktop, BlueZ and deterministic backends already did
  (`docs/UNIFIED_SEMANTICS.md` §§3 and 8; Android is the reference).
  `physicalLinks` counts peers: one link, N leases.
  `connection.already-owned` now means only a transitional teardown or a
  different owner. The TCK pins are updated.

- **`when-available` refusal matches the capability.** A backend without
  presence semantics refuses `connection:connect` with intent
  `when-available` as `capability.unsupported`, and
  `supports('connection:when-available')` agrees on every leg.

- **Overflow notices are cumulative restatements.** Every `stream.overflow`
  states the cumulative-so-far `droppedItems` and `droppedBytes`,
  non-decreasing, with the last one stating the gap. Single-notice and
  multi-notice backends satisfy the same rule.

- **Electron answers `connection.effective-mtu`.** The main router serves
  the renderer's MTU observation like the Tauri route.

- **Departure from 4.x — a connect whose link the platform could not
  establish is `caller-decides` on every host.** A physical Android run
  (Samsung, Polar H10) failed `mtu` twice with GATT status 133 reported
  `retryability: 'never'`, although nothing had been committed. (Motivation,
  not a support claim.) The failure is now `connection.failed` on every
  host, keeps the platform's own answer in `platform` and reports
  `caller-decides`; the library never retries it itself.
  One rule for every backend (`is_transient_establishment_failure`,
  `crates/ubm-desktop/src/errors.rs`, applied by the central's connect):
  Android GATT 133, 62 (HCI 0x3E) and 147; CoreBluetooth `CBErrorDomain`
  6 and 10 (iOS and macOS); WinRT `Unreachable`; BlueZ
  `org.bluez.Error.Failed` / `ConnectionAttemptFailed`; Web
  `gatt.connect()` rejected with `NetworkError`. Every other connect
  failure stays `never`. Before and after:

  | Backend                     | Platform answer                                   | 4.x / rc.0                                               | 5.0                                   |
  | --------------------------- | ------------------------------------------------- | -------------------------------------------------------- | ------------------------------------- |
  | React Native Android        | `androidGattStatus` 133 / 62 / 147                | `platform.failure`, `never`                              | `connection.failed`, `caller-decides` |
  | React Native iOS            | `CBErrorDomain` 6 / 10                            | `platform.failure`, `never`                              | `connection.failed`, `caller-decides` |
  | Node/Electron/Tauri macOS   | `corebluetooth` 6 / 10                            | `connection.failed`, `never` (the `NSError` was dropped) | `connection.failed`, `caller-decides` |
  | Node/Electron/Tauri Windows | `gatt-status` `unreachable`                       | `connection.failed`, `never` (no platform answer)        | `connection.failed`, `caller-decides` |
  | Node/Electron/Tauri Linux   | `bluez-dbus` `Failed` / `ConnectionAttemptFailed` | `platform.failure`, `never`                              | `connection.failed`, `caller-decides` |
  | Web                         | `NetworkError`                                    | `connection.failed`, `never`                             | `connection.failed`, `caller-decides` |

  Wire and native changes:
  - every `ubm-mobile-wire/1` failure envelope carries the owner's
    `retryability` (the provider used to re-derive it from the code, so a
    `caller-decides` the owner reported was lost on mobile);
  - `crates/ubm-mobile/golden/wire-vectors.json` regenerated, with a new
    GATT 133 connect vector; the TS parser refuses an envelope without it,
    or a `caller-decides` write whose commit is `uncertain`;
  - vendored btleplug patch 15: CoreBluetooth `didFailToConnect` keeps its
    `NSError`, and a WinRT connect answered `Unreachable` keeps that answer;
  - the Tauri transport accepts `caller-decides` on `connection.connect`.
    Native rebuilds: the iOS app (`RustCore.xcframework`), the Android app
    (`jniLibs`), the Node N-API addon and the Tauri plugin.

- **Departure from 4.x — one name per physical event on every host
  (reverses finding 132's Apple rule).** The same event now carries the same
  error code, lifecycle transition and stream terminal on React Native
  Android and iOS, Node/Electron/Tauri on macOS, Windows and Linux, and
  Web; Android is the reference wherever a platform can do the same, and
  the platform's own answer stays in `platform`. The table lives in
  `src/backend-contract/event-vocabulary.ts`, is embedded in
  `docs/UNIFIED_SEMANTICS.md` ("One name per physical event") and is
  pinned by `__tests__/event-vocabulary.test.js` and the Rust tests
  `crates/ubm-desktop/tests/event_vocabulary.rs` and
  `crates/ubm-mobile/tests/event_vocabulary.rs`. Before and after:

  | Event                               | Backend                 | 4.x / rc.0                                                                                                                                                                                     | 5.0                                                                                                                                                                                                   |
  | ----------------------------------- | ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | Link lost during an operation       | RN iOS                  | `platform.failure` (finding 132)                                                                                                                                                               | `connection.lost`                                                                                                                                                                                     |
  |                                     | macOS / Windows / Linux | the operation's code (`gatt.read-failed`, `platform.failure`, `gatt.discovery-required`), `operation.disconnected` for a late read, or `operation.timed-out` when CoreBluetooth never answered | `connection.lost`, at once                                                                                                                                                                            |
  |                                     | Web                     | `operation.disconnected` (and `NetworkError` → `operation.disconnected`)                                                                                                                       | `connection.lost`                                                                                                                                                                                     |
  | App release cuts an operation off   | RN Android / iOS        | `connection.lost` / `platform.failure`                                                                                                                                                         | `operation.disconnected`                                                                                                                                                                              |
  |                                     | Web                     | `operation.disconnected`                                                                                                                                                                       | unchanged                                                                                                                                                                                             |
  | Adapter lost during an operation    | Web                     | `operation.disconnected`; lifecycle `peer-link-loss`, stream `connection-lost`                                                                                                                 | `operation.reset`; lifecycle `adapter-loss`, stream `source-failed`                                                                                                                                   |
  | Connect not established             | RN Android / iOS, Linux | `platform.failure`                                                                                                                                                                             | `connection.failed` (and `caller-decides`, above)                                                                                                                                                     |
  | Peer never observed                 | Web                     | `connection.not-found`                                                                                                                                                                         | `peer.not-found`                                                                                                                                                                                      |
  | Authentication / encryption refused | RN Android / iOS, Linux | `platform.failure`                                                                                                                                                                             | `platform.security` (recovery: pair)                                                                                                                                                                  |
  |                                     | macOS                   | `gatt.read-failed` / `gatt.write-failed`                                                                                                                                                       | `platform.security`                                                                                                                                                                                   |
  |                                     | Windows                 | `gatt.read-failed` / `gatt.write-failed`                                                                                                                                                       | `platform.security` (recovery: pair) when the ATT error byte is present (`protocol-error` + attError 5/8/12/15); `gatt.read-failed` / `gatt.write-failed` when the write path yields no result object |

  Mechanics:
  - one rule set in `crates/ubm-desktop/src/errors.rs`, applied by the
    central for mobile and desktop alike: `is_link_loss_answer`
    (CoreBluetooth `CBErrorDomain` 3 / 7, WinRT `Unreachable`, BlueZ
    `NotConnected` or `Failed` "Not connected", btleplug's own
    `NotConnected`), `is_security_answer` (ATT 5 / 8 / 12 / 15, Android
    137, `CBErrorDomain` 14 / 15, BlueZ `NotAuthorized`,
    `AuthenticationFailed`, `NotPermitted` "Not paired"; only a generic
    failure is renamed, so Android status 8 at a disconnect stays a link
    loss), `classify_connect_failure`;
  - the central ends a link operation still waiting on the radio when the
    OS reports the link ended (`RadioEvent::Disconnected` / `Lost`) or the
    app's release starts, as Android's stack does, so a CoreBluetooth call
    that never answers no longer waits for its deadline; the result is
    `connection.lost`, or `operation.disconnected` when the app's release
    was underway;
  - iOS: `not-connected` is `connection.lost` on Apple too, the `NSError`
    (owned radio 1016 / 1020, `CBErrorDomain` 7) kept — this reverses
    finding 132, which kept legacy Apple's `platform.failure`: Android
    reported `connection.lost` for the same fact, so a supervisor retried on
    Android and stopped on iOS;
  - Web ends an operation with the link's own reason
    (`WEB_LINK_END_CODES`) and reports an adapter loss as `adapter-loss`.
    `crates/ubm-mobile/golden/wire-vectors.json` regenerated. Native rebuilds:
    both mobile `RustCore` builds, the Node N-API addon and the Tauri plugin.

- **Departure from 4.x — a connection supervisor reconnects when the link
  drops during `configure`.** The same run lost the link (status 22) while
  `configure` discovered services, and the supervisor stopped. A
  `configure` that rejects with `connection.lost` is now a link loss: the
  supervisor releases the connection, backs off and reconnects. One that
  rejects with `operation.reset` (the adapter went away) waits for the
  adapter, then reconnects. Any other `configure` failure, including
  `operation.disconnected` (the app's own release), still stops it. Shared
  public layer, so every host decides the same; the cross-backend test in
  `__tests__/event-vocabulary.test.js` feeds every backend's names through
  it (`docs/CONNECTION_MANAGER.md`).

- **Departure from 4.x — a dispatched connect whose deadline expires before
  any link came up is `connection.failed` on every backend (finding
  161).** The same physical event ("the peer did not answer the connection
  attempt") carried two names: `connection.failed` where the controller
  gives up (Android GATT 133/147) but `operation.timed-out` where the stack
  never fails a pending connect on its own (CoreBluetooth, btleplug,
  Web), so a supervised reconnect reported different codes per host. Both
  observations now report `connection.failed` (`caller-decides`, never
  retried by the library), with the deadline fact in the error `platform`
  detail (`core` / `deadline-expired` with `deadlineMs` on native,
  `web-bluetooth` / `DeadlineExpired` on Web, `ipc` / `deadline-expired`
  with `deadlineMs` where the shared IPC deadline wins the race against the
  native core on Tauri and Electron renderer). A caller-supplied AbortSignal
  abort stays `operation.aborted`; timeouts on other operations stay
  `operation.timed-out`. New vocabulary row `connect-deadline-expired`
  (`src/backend-contract/event-vocabulary.ts`, `docs/UNIFIED_SEMANTICS.md`,
  Rust fixture regenerated), covered on all six backends by
  `__tests__/event-vocabulary.test.js` and by central/Web mapping tests, and
  on the TS-enforced side by `__tests__/ConnectDeadlineVocabulary.test.js`
  (Tauri, Electron renderer and Web connect deadlines, plus abort and
  other-operation negative legs).

- **Departure from 4.x — a manager survives an adapter loss on every host,
  and a supervised connection reconnects when the adapter returns.** A
  physical run (Samsung phone, Polar H10, Bluetooth off 12 s) left
  `createConnectionSupervisor()` in `waiting-for-gate` after Bluetooth came
  back, and `manager.destroy()` then answered `release-failed`
  (`gatt.not-found`, `path.resolve`). (Motivation, not a support claim.)
  4.x (the legacy core, `applyAdapterStateEvent` and `backend-restarted` →
  `releaseResources('backend-restart')`) destroyed the manager on an adapter
  loss: every resource was released, the supervisor ended
  `lifecycle.destroyed`, and the application had to create a new manager. Now,
  on React Native Android and Apple, CoreBluetooth, WinRT, BlueZ, Electron and
  Tauri: every live connection ends `adapter-loss` (`connected → lost`), scans
  and subscriptions end `source-failed` (React Native had said
  `connection-lost`; `source-failed` is what legacy React Native, the desktop
  backends and Tauri said), in-flight operations settle `operation.reset`, and
  the manager binds the backend's new attachment (generations advance once per
  loss episode, `1`, `2`, …) and stays ready. Peer handles stay usable. A
  release of anything the loss ended answers `released`, as does `destroy()`
  (the mobile owner and `DesktopCentral` remember the leases and consumers a
  reset ended and answer them released once). The supervisor keeps waiting
  when its readiness wait times out or the adapter is still not ready (any
  `adapter.*` or `operation.timed-out` from the wait) instead of parking, so an
  outage longer than the 10 s readiness window still reconnects. A backend
  replaced by a different backend instance still ends the manager as 4.x did.
  Electron and Tauri renderers follow through IPC protocol 4 (below).
  Covered by deterministic off/on tests on each host, including an outage past
  the readiness window and a later link loss on the recovered connection.

- **Write receipts, IPC release and reconnect.** A write without response
  reports `commitState: 'unknown'` on every host, Tauri included; a renderer
  receiving `'accepted'` rejects it as `protocol.malformed`. An
  app-requested release over IPC (Electron renderer, Tauri) delivers
  `disconnected` / `requested-disconnect` and then the `owner-released`
  terminal, so `createConnectionSupervisor` makes the same decision as on
  React Native. Electron main bounds renderer release retries (30 × 100 ms);
  exhaustion reports a `release-failed` record and teardown disarms pending
  retries.

- **React Native manager cleanup is truthful.** A failing adapter-watch close
  fails `destroy()`/`stop()` with `release-failed` (the watch is retained for a
  retry) instead of reporting `released`. Errors thrown below `connect` that
  are not contract outcomes surface as themselves rather than as
  `connection.failed`. A failed release during a backend-failure teardown is
  recorded on the manager trace. The bridge's structural casts are replaced
  with verified shapes.

- **Retryability is the core's answer about dispatch, not something derived
  from the error code.** An aborted or timed-out operation that was
  dispatched and may have committed at the peripheral (a write, a
  descriptor write, an MTU request) is reported with `retryability: 'never'`
  and `commitState: 'unknown'`. Only an operation that never reached the radio,
  or one that commits nothing, is `caller-decides`. This applies to the
  TypeScript operation coordinator, the Tauri transport (which now accepts the
  native `never` for `operation.aborted`/`operation.timed-out` instead of
  rejecting it as malformed), and the IPC manager, whose deadline remap now
  rewrites only `operation.aborted` to `operation.timed-out` and keeps the
  native retryability, domain, operation and platform detail. Other native
  failures after a deadline are rethrown unchanged instead of being replaced
  by a timeout.

- **Electron main: a write or descriptor write that completed is reported
  with its receipt even when cancellation or the deadline arrived after
  dispatch,** as destructive cleanup already was. Previously the finished write
  was reported as aborted/timed-out `caller-decides`, inviting the caller to
  send it a second time.

- The operation coordinator no longer reports `commitState: 'unknown'` for a
  write refused before dispatch (pre-aborted, pre-expired, or admission
  closed); nothing reached the radio, so it is `not-applicable`.

- **Recovery advice follows the operation's retryability instead of the code
  alone.** An `operation.aborted` or `operation.timed-out` failure that is
  `never` retryable (a dispatched write whose commit is unknown) now advises
  `{ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] }`
  instead of `retry`. `caller-decides` failures and every other code keep
  their existing advice, and `recoveryForCode(code, operation)` is unchanged.

- **Recovery advice and the public error follow the owner's reported commit
  state.** When an error carries `commit: 'uncertain'` (a dispatched
  write that may have reached the peer), `recoveryForError` advises
  `caller-policy` with the code's prerequisite actions, no `retry`, and a final
  `{ kind: 'verify-state' }`, whatever the code. For example, a write that ended
  `operation.disconnected` now advises reconnect and then verify-state instead of
  `retry-with-backoff`. `commit: 'not-dispatched'` keeps the code's advice. The
  TypeScript operation coordinator now stamps `commit: 'uncertain'` on every
  failure of a dispatched write, and public cleanup failures keep the `commit`
  they were given instead of dropping it.

**GATT reads, MTU, PHY and write limits.**

- **Departure from 4.x — reading a characteristic while it notifies works on
  Apple, with the same app code as Android, and every read says what the
  platform knows about its value.** Subscribing to the Polar H10 PMD control
  point and then reading it worked on Android and failed on the iPhone with
  platform code 1031 ("Independent read is ambiguous while this characteristic
  is notifying"); macOS Node/Electron refused it the same way (btleplug patch
  #14, codes 413/414/415). The Apple radios now issue the read.
  CoreBluetooth reports a read response and a notification through one
  `didUpdateValueFor` callback, so the value is labelled instead of refused:
  - new `GattCharacteristic.readReceipt()` resolves `{ value, provenance }`
    (`GattReadReceipt`, `GattReadProvenance`); `read()` still resolves the
    value. `provenance` is one vocabulary on every backend:
    `read-response` — the platform attributed the value to this read's ATT
    response (Android `onCharacteristicRead`, WinRT `ReadValueAsync`, BlueZ
    `ReadValue`, Web Bluetooth `readValue()`, CoreBluetooth while the
    characteristic cannot notify); `read-or-notification` — CoreBluetooth
    while the characteristic notifies, has a subscription, or has a
    notification state change in flight: the value is this read's response
    or a notification/indication;
  - the value that completes such a read is still delivered to the
    characteristic's subscribers, so nothing is dropped from the stream; on
    Apple the stream can therefore also carry read responses;
  - reads of one characteristic queue and complete in request order (one
    `readValue` outstanding at a time on iOS); a second read no longer fails
    1011/414, and a subscribe while a read is pending no longer fails
    1032/415. A read cancelled or timed out after it was issued leaves its
    update owed, and that update never completes a later read;
  - timeouts, cancellation and disconnect settle a pending read exactly as
    before;
  - backend SDK: `GattBackend.read` resolves `CharacteristicReadResult`
    (`ReadResult` plus `provenance`) and `GattDatabase.read` resolves
    `CharacteristicRead` (`{ value, provenance }`); `ReadProvenance`,
    `READ_PROVENANCES` and `isReadProvenance` are exported. A backend that
    cannot attribute a value must answer `read-or-notification`, never
    `read-response`;
  - wire: the `ubm-mobile-wire/1` `gatt.read` reply carries `provenance`
    (`crates/ubm-mobile/golden/wire-vectors.json` regenerated); the radio
    host answers a characteristic read with the new
    `RadioCompletion::Read { value, provenance }` (UniFFI `Read`, JNI
    `nativeCompleteRead`), `Bytes` stays for descriptor reads; the N-API
    `UbmCentral.read` resolves `{ value, provenance }`; the Electron and
    Tauri `gatt.read` IPC answer carries `provenance`, and a renderer
    `readReceipt()` against a host that omits it fails `protocol.malformed`.
    The legacy Native Protocol v2 path and the legacy Electron CoreBluetooth
    addon, both unreachable from every package export, keep refusing: their
    wires cannot carry a provenance. Native rebuilds: the iOS app (Swift radio
    and `RustCore.xcframework`), the Android app (`jniLibs` for
    `nativeCompleteRead`) and the Node N-API addon.

- **New in 5.0 — React Native answers `gatt:maximum-write-length`.**
  `connection.controls.maximumWriteLength(mode)` and
  `database.maximumWriteLength(path, mode)` on Android and iPhone now report
  the platform's own answer through the Rust owner (new wire op
  `connection.maximum-write-length`, `docs/MOBILE_RUST_WIRE.md`) instead of
  `capability.unavailable`. iPhone reports `CBPeripheral
.maximumWriteValueLength(for:)` per type. Android reports 512 with response
  (the stack performs the prepared/long write; `BluetoothGatt` refuses more
  from API 33) and MTU − 3 without response, from the MTU `onMtuChanged`
  reported, or 20 (ATT default MTU 23) before any exchange. The capability is
  `limited`, with limits 1–512 bytes and platform-named limitations
  (`android-att-default-mtu-before-exchange`,
  `android-prepared-write-with-response`, `live-radio-qualification-pending`).
  The borrowed `mobile-maximum-write-length-unavailable` limitation is gone.
  With the capability executable the core also registers its emulated
  `gatt:long-write` on React Native. Both React Native TCK legs now run the
  `tck.feature.gatt.maximum-write-length` suite. The deterministic React
  Native module reports no effective MTU before `connection.request-mtu`, as
  Android's `readEffectiveMtu` does. Needs a native rebuild (the Rust owner
  changed); the Kotlin and Swift adapters already answered `ReadWriteLimits`.

- **`connection.controls.effectiveMtu()` no longer throws a raw
  `TypeError`.** The public controls read the connection's `effectiveMtu`
  (and `writeWithoutResponseReadiness`) into a local and called it detached,
  so a connection whose method reads its own receiver — the React Native Rust
  connection, and the legacy manager connection before it — failed with
  `Cannot read property 'control' of undefined` instead of answering. Seen on
  a physical Android against a Polar H10, before and after `requestMtu(517)`.
  (Motivation, not a support claim.) Both controls now call the connection
  method on its receiver; Android answers the measured ATT MTU (or
  `unavailable` before any exchange) and iPhone answers
  `capability.unsupported` with `corebluetooth-effective-mtu-unavailable`,
  as legacy registered them.

- **`effectiveMtu()` accepts caller operation options, matching `readRssi`
  (pre-1.0 contract change on an unreleased 5.0).**
  `BleConnectionControls.effectiveMtu()` now accepts caller `OperationOptions`
  (`signal`, `timeoutMs`) like every other link control, across the public
  contract and every backend. On the React Native Rust route
  `connection.effective-mtu` now carries `operationId` with optional
  `budgetMs`, so a caller `AbortSignal` cancels an in-flight observation and a
  deadline bounds it.

- **Breaking — `NotificationValue.indication` is replaced by
  `delivery: 'notification' | 'indication' | 'unknown'`.** A backend reports
  the delivery its platform reported and `unknown` when the platform does not
  report it (CoreBluetooth, BlueZ), instead of a `false` that read as
  "notification". WinRT and Android report the CCCD mode they wrote; Web
  Bluetooth reports the mode its specification selects. The internal
  `PortableNotificationValue.indication` field is removed with it.
  **Platform permissions, diagnostics and packaging.**

- **Departure from 4.x — Apple `permissions.request` presents the system
  Bluetooth prompt (finding 179).** The 4.x Expo bridge refused with
  `capability.unsupported` (`unsupportedPermissionPrompt`) while readiness
  reported a `request-permission` action, so an app that followed the
  library's own advice was stuck. On Apple (iOS/tvOS)
  `manager.permissions.request({ purpose: 'scan-and-connect' })` now does
  what Android's does: it presents the CoreBluetooth prompt through the
  process-owned radio and reports the same shape (`requested`, `granted`,
  `denied`, `recommendedSettingsTarget`). The prompt appears on request,
  never from reading `manager.readiness()`: the process central is allocated
  by the request (or, with a restoration id configured, at startup as
  `willRestoreState` requires). An already-decided authorization answers at
  once; the request accepts `timeoutMs` and `signal`
  (`operation.timed-out`/`operation.aborted`, `300000ms` native bound
  otherwise); a restriction (parental controls/MDM) refuses
  `capability.unsupported` with its reason. The Expo example driver follows
  readiness actions generically instead of assuming Android.

- **Departure from 4.x — the React Native diagnostics and adapter limitation
  wording stays truthful to the Rust route (finding 159).** The 4.x providers
  reported the JSI protocol boundary on Android and Apple, with transport
  `native-protocol-v2` and a limitation naming the canonical JSI protocol
  boundary as the adapter's exposure. The 5.0 Rust route reports what it
  actually uses — boundary `ubm-mobile-wire/1`, transport `native-core-session` —
  and the limitation `The process-owned Rust mobile owner schedules every radio operation; this backend holds no TypeScript radio policy`. The wording differs
  from legacy on purpose: repeating the 4.x words would describe a transport
  that no longer exists on this route. Adapter display names are unchanged.

- **Removed the `dbus-next` peer dependency (finding 101).** No production
  entrypoint loads `dbus-next`: the BlueZ path executes the shared Rust core
  and the D-Bus boundary module stays only as an unreachable parity reference
  until Phase 4 deletion. Consumers no longer install it; it remains a
  development dependency until the reference and its tests are deleted.

- **Desktop test seams moved to `unified-ble-manager/testing` (finding 100).**
  The production desktop factory `createDesktopRustCoreBackendProvider` no
  longer accepts `radio`, `loadBinding`, `hostPlatform` or
  `firstStateTimeoutMs` (passing one is `argument.invalid` before anything
  loads), and the desktop entrypoints no longer export
  `DESKTOP_RUST_CORE_PARITY`. Deterministic suites use
  `createTestDesktopRustCoreBackendProvider` and `DESKTOP_RUST_CORE_PARITY`
  from `unified-ble-manager/testing`, which serve the synthetic radio.

- **Precompiled Rust artifacts now have a managed lifecycle.** Every Rust
  change used to stale up to three precompiled artifacts, each found late
  and by hand; now `pnpm native:status` reports each one as `fresh`,
  `stale`, `missing` or `not-applicable` with its staged and current digests
  and its exact refresh command, and `pnpm native:refresh [--only
android,apple,desktop]` rebuilds only what is stale with the canonical
  builders. Every consumer refreshes what it needs itself before it builds
  or launches — `hosts.sh up` (desktop N-API for electron/node), `build-tv.sh
build` and the phone Expo builds (Apple RustCore / Android jniLibs) — and
  aborts on a stale artifact; `UBM_NATIVE_REFRESH=off` switches to
  check-only. The committed Android `jniLibs` refresh the same way and stay a
  visible git diff. See `docs/NATIVE_ARTIFACTS.md`.
  For the Expo iOS fixture, the build also refreshes its pnpm `file:..` copy
  when stale and verifies the copied RustCore that Xcode links. It checks the
  generated app restoration settings before Xcode starts.

- Backend `diagnostic-warning` events now reach the public API (finding 102).
  Before, the desktop and React Native Rust providers emitted them but neither
  `UnifiedBleCore` nor the React Native manager handled them, so a user never
  saw them. Both managers now record each one in the diagnostic trace
  (`manager.diagnostics.snapshot().trace`). The record's kind is `attachment`,
  its event is `diagnostic-warning:<code>`, and its cause is the error code
  the backend reported, or `null`. Facts that have a typed home are no longer
  reported only as diagnostics:
  - React Native: advertisements and notifications the owner dropped at a
    full native queue count in the drop accounting of every scan or
    notification stream that could have received them. Advertisement counts
    now add up across drops; before, each drop replaced the last count.
  - React Native: when a control record (`link`, `scan-end`, `adapter`) is
    lost, the owner is asked again (`adapter.state`, `peers.connected`,
    `counters.describe`). A link the owner no longer reports ends as
    `connection-lost` and its subscriptions end with it. A scan membership
    the owner no longer holds ends `source-failed`. Adapter watches get the
    current state.
  - Desktop: a malformed advertisement counts in the scan's drop accounting.
    A scan or notification pump that fails ends its streams `source-failed`.
    A failed core event-pump turn re-reads links, security, write readiness,
    the owned scan and adapter power. A core event stream that closes while
    the backend is live fails the backend event stream, so the manager
    releases what it holds. The core also stops spinning on a closed stream.

- **Desktop distribution:** the package ships verified prebuilt desktop-core
  addons under `native/desktop-core/prebuilds/<platform>-<arch>/` for
  linux, darwin and win32 on x64/arm64. Each has an identity sidecar (the
  file's sha256 plus the binary's own build identity). The loader finds the
  addon only from its own location, with no cwd lookup, no `linux-x64`
  fallback, and no relative `UBM_NAPI_ADDON`. Before any radio call the host
  checks `nativeBuildIdentity()` against the sealed expectation
  (`protocol.incompatible` / `<host>.native-identity`). Linux needs glibc
  2.35+ and `libdbus-1.so.3`; musl reports `no-prebuilt-for-target`.
- BlueZ `busKind` now reaches the core instead of being validated and then
  dropped. `pairingGeneration` is carried to the core through a host
  controller bridge instead of being refused.
- `docs/ELECTRON.md`: bundlers must keep `native/desktop-core` external and
  unpacked from ASAR. Renderer deadlines cross to main as a relative
  `budgetMs`.

- Native build mode is explicit and identical on iOS and Android:
  `UBM_NATIVE_BUILD` unset or empty means `prebuilt`, `prebuilt` and `source`
  are explicit, and any other value now fails `pod install`
  (`Pod::Informative`) and Gradle configuration (`GradleException`).
  Previously the podspec treated every value except `source` as prebuilt, and
  Gradle silently switched to source mode when Rust sources and `../.git`
  existed; that inference is removed. The podspec no longer uses
  `prepare_command` (CocoaPods never runs it for `:path` pods): source mode
  builds `ios/RustCore` beforehand with `pnpm native:apple:prepare`, and
  `pnpm native:android:prepare` pre-builds the Android ABIs.
- Native build identity: `scripts/release/native-build-identity.js`
  computes, per binding (napi, jni, uniffi), a source digest over the crate
  and its transitive path dependencies and a binding-schema digest over the
  wrapper-side declarations (T1). Builders embed both in the binaries through
  each binding's `build.rs` (`ubm-native-build-identity/1`; `"unsealed"`
  without them), `src/generated/native-build-identity.ts` carries the
  expected values (`prepack` fails when it is stale), and the build
  fingerprint seals both. The Apple staging now writes
  `ios/RustCore/build-identity.json` and is verified by
  `ios/verify-rust-core.sh` (exact slice set parsed from Info.plist, every
  hash) instead of counting `LibraryIdentifier` lines; the committed Android
  identity is `android/src/main/jniLibs/build-identity.json` (replacing
  `build-identity.txt`). The publish workflow rejects Apple or Android
  artifacts not built from the tagged sources. The currently committed
  Android prebuilts are verified against the same sealed sources before release.

**Identities, TCK and discovery.**

- **React Native, desktop and Tauri report their 4.x resource identities
  again.** React Native public operation correlations are `operation-{n}` per
  manager again (the wire keeps its own ids internal). The Node/Electron
  desktop provider names its instance, attachment, generations, adapter
  display name, scans, peers, connections, leases, databases and subscriptions
  as each OS's 4.x backend did. Tauri numbers `tauri-attachment-{n}`,
  `tauri-btleplug-{n}`, `tauri-backend-generation-{n}`,
  `tauri-adapter-generation-{n}`, leases and connection identities from the
  plugin's one counter, as Tauri 4.x did; a reset advances them the same way.
- **Desktop provider-minted operation correlations read `operation-{n}`
  again.** The desktop Rust provider labelled the operations it mints
  itself (scan, connect, discover, GATT verbs on database handles, …)
  `{platform}-core-{kind}-{n}`, while the legacy core mints
  `operation-{n}` for the same receipts and dispatch handles. Both now
  read `operation-{n}`, numbered from the provider's per-backend counter.
  Pinned by `provider-minted operation correlations keep the legacy
operation-{n} shape` in
  `__tests__/backends/desktop/desktop-rust-core-provider.test.js`.
- **The shared Rust central names no host; the React Native Rust route
  reports the legacy React Native identity.** `DesktopCentral` minted every
  attachment as the desktop host (`desktop-backend-gen-{n}`,
  `desktop-adapter-gen-{n}`, `ubm-desktop-mobile-{platform}-{owner}`), so on
  Android and iPhone `manager.readiness()` and every adapter record reported
  `backendGeneration: "desktop-backend-gen-1"`. The owning host now supplies
  its identity (`HostIdentity` in `CentralProfile`, replacing its `owner` and
  `backend_label` fields): the Node/Electron addon and Tauri pass
  `DesktopIdentity`, which keeps their names unchanged, and the mobile owner
  passes `MobileIdentity`, which uses the legacy React Native formats —
  generations `"1"`, instance `react-native-{android|apple}-backend-{n}`,
  attachment `{instance}:{gen}:{gen}`. The central's own open failures and log
  lines use the host's namespace (`ubm-mobile.host.open`, `ubm-mobile:`). The
  React Native backend's attachment (`react-native-{platform}-backend-{n}`,
  `{instance}:1:1`), Apple adapter display name (`Apple CoreBluetooth central
adapter`) and public resource names (`corebluetooth-peer-{gen}-{n}`,
  `corebluetooth-scan-session-{n}`, `corebluetooth-connection-{n}`,
  `corebluetooth-connection-generation-{n}`, `corebluetooth-connection-lease-{n}`,
  `corebluetooth-database-{n}`, `corebluetooth-database-generation-{n}`,
  `corebluetooth-subscription-{n}`, each counter per backend from 1) are the
  legacy ones again. An empty mobile `HostOptions.owner` now fails with
  `ubm-mobile.host.owner` instead of `desktop.owner`. The golden wire vectors
  are regenerated; they and a TS guard fail on any desktop name.

- **Breaking — the desktop first-party TCK legs run the Rust route.**
  `createCoreBluetoothFirstPartyTckRegistration`,
  `createBluezFirstPartyTckRegistration` and
  `createWinRtFirstPartyTckRegistration` in `unified-ble-manager/testing` keep
  their names but now drive the production desktop provider over the
  identity-verified N-API addon on its synthetic radio. They take
  `{ now, binding?, nativePeerId? }` (absent `binding`, the packaged addon is
  loaded as the production factories load it); the legacy `createBoundary`,
  `busKind` and `selectedAdapterId` options are gone, and the
  `Deterministic*Boundary` / `BluezNotificationInput` types now name the
  addon's synthetic staging surface (`DesktopRustCoreSyntheticRadio`). Each leg
  runs its 4.x scenario set with the 4.x exclusions, plus the feature suites
  its Rust provider registers. The legs exposed, and this release fixes, four
  desktop regressions against 4.x: `attach` negotiates the caller's core offer
  again (a skewed offer is `protocol.incompatible`, a malformed one
  `protocol.malformed`) and refuses a second attach; `events()` returns one
  stream per caller, so a borrowing manager no longer shares or ends the
  owner's; a second connect to a live link was refused
  `connection.already-owned` on CoreBluetooth and WinRT while BlueZ joined (as
  dbus-next did) — later in this release every backend joins, see "Same-peer
  connects join on every backend"; and `security.watch` opens with the peer's
  current state. `resourceCounters()` `dispatchedOperations` now counts
  operations in flight, not a running total.

- **Breaking for TCK consumers: new base scenario
  `gatt.duplicate-uuid-occurrences-route-exactly`** (finding 99), with facts
  `gatt-duplicate-uuid-occurrences-are-indexed-per-parent` and
  `gatt-duplicate-uuid-notifications-route-to-exact-instance`. It needs a
  fixture world that has a second service UUID and a same-UUID occurrence 1 at
  the service, characteristic and descriptor levels, and it refuses a world
  without one rather than passing vacuously. Discovery must number each
  repeated UUID under its parent in discovery order
  (`docs/UNIFIED_SEMANTICS.md` §9), and a notification addressed to one
  instance must reach only the subscription on that instance's complete path.
  Every first-party leg runs it: the desktop Rust legs over the real addon,
  React Native Android and Apple, Web (through the chooser), and the
  deterministic backend. The Rust parity driver pins it as a staged program.
  The occurrence checks in `gatt.discovery-complete-paths-and-services-changed`
  now key each level by UUID and occurrence; before, two services with
  different UUIDs both at occurrence 0 counted as a duplicate. The
  deterministic virtual peripheral, the React Native owner double and the
  in-memory Web boundary now serve the duplicate-UUID world after their
  original attributes. The in-memory Web chooser now grants only the services
  a request names, as a browser does. The owner double's `emitNotification`
  takes an optional exact instance. A desktop test over the real addon ported
  from the 4.x duplicate-UUID public-path tests proves discovery order,
  occurrences, and instance-exact reads, writes, descriptor accesses and
  notifications on every desktop leg.

- A discovery registers the whole database or fails; it never skips entries
  (finding 95). The desktop N-API `discover` answer is now
  `{ pathsRegistered }` (`DiscoveryInfo.skipped` and `SkippedEntryInfo` are
  gone). The React Native wire `gatt.discover` value is exactly
  `{ connectionGeneration, databaseGeneration, services }`, and an answer
  that still carries `skipped` is refused `protocol.malformed`. The Tauri
  `gatt.discover` response has no `skipped` either. The desktop provider no
  longer emits `discovery-entry-skipped`, and React Native no longer emits
  `discovery-skipped-attributes`. The core's typed refusal reaches the
  manager unchanged on desktop, React Native and Tauri, with no partial
  database:
  - a malformed platform UUID is `protocol.malformed` / `gatt` /
    `discovery.snapshot.uuid`;
  - a database past the ATT handle space (65535 attributes) is
    `capability.limited` / `gatt` / `discovery.database-bound`;
  - a discovery without a core lease is `argument.invalid` / `core` /
    `path.owner`.
    The legacy backends also failed a malformed UUID whole.

- The desktop first-party TCK legs take the `already-paired` answer for a
  bonded peer from the core's synthetic radio. The harness no longer
  fabricates it (finding 91).
- The N-API `DispatchRadio` forwards `os_answers_unflagged_subscribe` to the
  production and synthetic radio (finding 98). The trait default had kept a
  subscribe property gate on Linux that the legacy BlueZ backend never had. A
  guard test now fails if any `RadioBoundary` method is not forwarded to both
  inner radios. Two synthetic-only N-API staging hooks,
  `stageCharacteristicValue` and `stagedGattAccesses`, are refused on a
  production central like every other staging hook. They prove per-instance
  GATT routing over the real addon, and they are not part of any TypeScript
  entrypoint.
- The desktop provider reconciles a lagged core event stream instead of only
  logging it. A lifecycle gap re-reads every live link from the core
  (`UbmCentral.peerRecords`) and emits the missed `connection-lost` /
  `database-changed` (or the adapter-loss sequence while the adapter is lost);
  a scan-terminal gap re-reads the owned scan (`UbmCentral.activeScanId`) and
  ends a scan the core no longer owns `source-failed`; an adapter-reset gap
  also ends links and scan as the reset did; security and write-readiness gaps
  re-read each watched peer. `DesktopRustCoreBackend.settleCoreEvents()`
  applies the queued core events on demand.
- Desktop notification values the core still holds when a link is lost, the
  database changes or the adapter is lost now reach the stream, in order,
  before its terminal. Before, they were dropped uncounted. The addon now
  wakes the provider when it queues work (`UbmCentral.setEventWaker`, a
  coalesced, unref'd thread-safe function), so notifications, advertisements,
  lifecycle, adapter, reset, security, write-readiness and scan-end reports
  all arrive by event, as the 4.x native callbacks did. The polling intervals
  remain only as a safety net.
- Desktop notifications lost before the core could hold them are no longer
  silent (finding 131). The subscription's `delivery.overflowPolicy` now
  reaches the core consumer (`UbmCentral.subscribe` `overflowPolicy`; before,
  every core consumer was `error`, so a lossy subscription ended on the first
  core-side loss). `error` ends the stream with an `overflow` terminal
  carrying the core's counts, including radio-side loss; lossy policies
  surface the cumulative counts (`UbmCentral.consumerCounters`) as an overflow
  notice and keep delivering. A stream ended by a link loss, a database
  change or an adapter loss now keeps the values it already accepted ahead of
  its terminal, even when nobody was reading when it ended.
- A desktop scan reports only what the radio saw during that scan (findings
  121/122). The core queues sightings only while a scan is live and clears
  them when a scan starts or stops. The provider now takes
  `UbmCentral.takeScanObservation()` (the observation, its scan's core
  operation id and its age) and refuses any observation queued for another
  scan, with a `scan-observation-foreign` diagnostic. Observations are stamped
  when the core received them, not when the host took them, and labelled with
  what they are: `platform-derived` for the OS's merged device state (BlueZ
  `Device1`, the known-device report at scan start) and for CoreBluetooth and
  BlueZ advertisements, and `platform-raw` only for a WinRT advertisement's
  own data, as the 4.x backends labelled them. Before, every observation was
  `platform-raw`, stamped at take, and a new scan could receive the previous
  scan's tail and cached BlueZ devices.
- **Desktop error operation ids are the 4.x ids again.** Public errors from
  the desktop Rust path report each host's 4.x operation id
  (`direct-gatt.*` on CoreBluetooth, `winrt.*`, `bluez.*`), named after the
  operation in flight: `direct-gatt.connect`, `winrt.gatt.database-read`,
  `bluez.gatt.read`, `winrt.security.pair`. They no longer report the core's
  internal operation (`gatt.read`, `discovery.snapshot.uuid`, ...), which now
  rides `platform.metadata.coreOperation` of the `core-detail` detail, or the
  `<platform>-rust-core.*` / `<platform>-manager.*` ids of earlier 5.0
  builds. Load failures use the 4.x native-boundary ids:
  `<host>.native-boundary.load` (wrong OS, missing or unloadable addon, bad
  `UBM_NAPI_ADDON`), `<host>.native-boundary.version` (identity mismatch) and
  `<host>.native-boundary.create`; adapter selection is
  `<host>.provider.select-adapter`. `DesktopRustCoreProfile.operationPrefix`
  is now the 4.x prefix, and `desktopRustCoreOperation(prefix, name)` maps a
  provider operation to its id.
- Desktop core errors carry the OS's own answer as the 4.x platform identity
  again: CoreBluetooth `{domain:'corebluetooth', code:<NSError code>}`, WinRT
  `{domain:'winrt', code, metadata:{hresult, gattStatus}}`, BlueZ
  `{domain:'bluez-dbus', code:<D-Bus error name>}`. The N-API error wire is
  now `ubm-napi-error/3`, with a platform JSON field.
- A malformed desktop notification value ends that subscription's stream
  `source-failed` with `protocol.malformed`, as React Native does, instead of
  being dropped.
- A desktop scan's `localNamePrefix` reaches the OS filter again: BlueZ
  receives it as the `SetDiscoveryFilter` `Pattern`, as the 4.x dbus-next
  backend sent it; the software match stays the final filter.
  `createElectronMainBluezBackendProvider` now takes the Node BlueZ factory's
  `busKind` and `pairingGeneration` options.
  **Desktop and mobile IPC, transports and parity.**

- **Breaking — the desktop IPC protocol is now version 4** for both the
  Electron renderer/main pair and the Tauri webview/plugin pair. The wire
  carries relative `budgetMs` deadlines, optional `commit` on normalized
  errors, subscribe `delivery`, connection-lifecycle events and, new in 4,
  the host-announced attachment rebind: after an adapter loss Electron main
  or the Tauri plugin (never a renderer or webview) rebinds every active
  lease to the backend's new attachment and announces it on the reserved
  `attachment` stream as `{kind: 'value', value: {kind:
'backend-restored', schemaVersion: 1, previousAttachmentId, attachmentId,
attachment}}`. The renderer/webview client adopts only an announcement for
  its own lease that names the attachment it holds, on the same backend
  instance, and refuses and reports anything else. Until the announcement,
  and for the replaced attachment afterwards, work is refused `backend.reset`
  before any radio effect, except releases (`operation.cancel`, `scan.stop`,
  `gatt.unsubscribe`, `gatt.database.release`, `connection.disconnect`,
  `connection.events.unsubscribe`); a Tauri route naming an attachment the
  plugin never gave fails `protocol.violation`. Tauri connection events keep
  reporting the attachment the link lived on. Each side offers exactly 4 (the
  renderer/webview client offer, Electron main, the Rust plugin, and the
  public `TAURI_PLUGIN_COMPATIBILITY.ipcProtocol`, now `4`), so a mixed pair
  where one side speaks protocol 3 or older is refused at bootstrap with
  `protocol.incompatible` in either direction, before any lease or operation.
  A Tauri plugin that selects any other version is refused with a
  `protocol.incompatible` `BleError` (`tauri-manager.ipc-protocol`) and
  released. Upgrade the npm package, the Electron preload/renderer bundle and
  main, and the Tauri crate together.
- **Breaking — React Native and Expo run only the Rust mobile owner.**
  Every factory in `unified-ble-manager/react-native` and
  `unified-ble-manager/expo` drives the process-owned Rust core through the
  `UnifiedBleRustCore` TurboModule (`ubm-mobile-wire/1`,
  docs/MOBILE_RUST_WIRE.md); nothing on the production path loads the legacy
  `UnifiedBleProtocolControl` module or the TypeScript providers.
  - Removed: the `legacyTypeScriptCore` and `control` options of
    `createReactNativeBleManagerWithEnvironment` (passing either now fails
    `argument.invalid` instead of being silently rerouted),
    `getNativeUnifiedBleProtocolControl`, the protocol-control type exports,
    and the seam functions `admitReactNativeRustCoreSession`,
    `dispatchReactNativeRustCoreOp` and `openAdmittedRustCoreSession`.
    `createReactNativeAndroidBackendProvider` and
    `createReactNativeAppleBackendProvider` keep their names and now build the
    Rust route; their options drop `control` and add `rustCore`,
    `androidApiLevel` (Android) and `restorationAuthority` (Apple).
  - Added: `createReactNativeRustCoreBinding`, the `randomBytes` and
    `restorationIdentity` methods of the `UnifiedBleRustCore` spec (host
    entropy and the Info.plist/manifest restoration identity), and
    `ReactNativeBleManagerOptions.androidApiLevel`/`restorationAuthority`.
  - Admission: the binding checks the binary's `nativeBuildIdentity()`,
    `contractRevision()` and `wireRevision()` against the sealed identity
    before any session opens, checks the admission record again, and closes
    the lease on every later failure; a failed close is reported as cleanup
    debt with the original error.
  - Delivery: one wake-driven, single-flight drain replaces the three 5 ms
    polling pumps; an idle manager makes no native calls. Retained bytes are
    charged at their actual size, and closed watches, event streams and wake
    subscriptions deregister.
  - Truth: write receipts and failure commit states are the owner's;
    notification `delivery` is what the platform reported; generations are
    the core's, and a `link`, `db-changed` or generation-changing `adapter`
    record invalidates older handles before any native I/O. Strict parsing
    replaces `Math.floor` coercion.
  - Cleanup: scan stop, unsubscribe, disconnect, dispose and background
    release keep their native identity until the owner confirms release; a
    `release-failed` record is returned verbatim and can be retried.
  - Parity: every capability the legacy React Native routes registered is
    registered in the same state and executes through its wire op (connection
    controls, PHY from Android API 26, security, bonded peers, address
    targeting, scan platform options, background and companion leases, Apple
    state restoration adopted with the legacy journal's rules). Android adds
    `security:cancel-pairing`.
  - Expo's background, companion and restoration surfaces run on the
    manager's Rust session (`background.*`, `companion.associate`,
    `peers.restored`); owner failures keep their contract code.
  - Evidence: deterministic only (a wire-level `UnifiedBleRustCore` double
    under the production binding and serializer). The React Native first-party
    TCK legs run the Rust route. Physical-radio proof is pending on devices.

- **Breaking — the desktop entrypoints now run the shared Rust core.**
  `createCoreBluetoothBleManager`, `createWinRtBleManager`,
  `createBluezBleManager`, the `createNative*BackendProvider` /
  `createDbusNextBluezBackendProvider` providers, and the Electron-main
  providers (including the new `createElectronMainBluezBackendProvider`) all
  execute `DesktopCentral` through one N-API addon. They are one provider,
  `createDesktopRustCoreBackendProvider({ platform })`, exported from every
  desktop entrypoint. Each factory refuses the wrong OS before anything loads
  (`<host>.platform`, `{linux,macos,windows}-required`).

- Desktop Rust path parity. New narrow OS adapters in `src/os/` sit behind
  `RadioBoundary`:
  - Linux (BlueZ over zbus): pair, cancel pairing and unpair; the just-works
    `Agent1` is registered only when a pairing is requested; bond-change and
    services-changed events; `ConnectDevice` or discovery for address
    targeting; `Adapter1.Powered` read as a fact; characteristic `Flags`;
    the negotiated MTU. BlueZ performs long writes for requests, so a
    request carries a whole attribute value.
  - Windows (WinRT): `DeviceInformationPairing` pair, cancel pairing and
    unpair; `GattSession.MaintainConnection` held per connection;
    `GattServicesChanged` events; adapter listing and selection by native id.
  - macOS: `CBManager.authorization`, and `maximumWriteValueLength` per write
    type through a vendored, minimally patched btleplug (`vendor/btleplug`,
    `UBM_PATCHES.md`).

  Capabilities are now registered per OS. Connected RSSI is reported
  unsupported on Windows and Linux, where btleplug only has advertisement
  RSSI. An unnamed adapter open with several adapters present fails with
  `adapter.ambiguous`. A subscribe's CCCD mode is decided from the
  characteristic's properties and the platform rule before any effect.
  Also added:
  - macOS: advertisement solicited and overflow service UUIDs and the
    connectable flag (`PeerSnapshot::extras`).
  - Windows: adapter authorization, read through `DeviceAccessInformation`.
  - BlueZ: the D-Bus bus choice (`CentralProfile::bluez_bus`, the legacy
    `busKind`), backed by a vendored bluez-async that can connect on the
    session bus.

- **Departure from 4.x — a BlueZ failure keeps the operation's own name
  (Linux gate).** The legacy BlueZ route answered every D-Bus method error
  as `platform.failure` (`normalizeBluezFailure`), so on Linux a refused
  subscribe/read/write/discover or a failed adapter listing lost its own
  code. The Rust core now attaches the `bluez-dbus` answer in `platform`
  without renaming: GATT verbs keep `gatt.read-failed` /
  `gatt.write-failed` / `gatt.subscribe-failed`, discovery keeps its code,
  a listing failure is `adapter.unavailable`, and a connect keeps
  `connection.failed`. A genuine link loss (`NotConnected`, `Failed` "Not
  connected"), security refusal or transient establishment failure is still
  renamed by the central's classify chain, on every OS. Pinned by
  OS-independent mapping tests in `crates/ubm-desktop`
  (`a_bluez_failed_answer_keeps_the_operation_name_on_every_os`).

- Desktop Rust route: legacy reconnect, disconnect, BlueZ, WinRT and
  CoreBluetooth parity (findings 127/135–138/141). Every behaviour below
  matches the legacy backend it replaces, with the same outcome and the same
  error code, domain and operation id:
  - Reconnect without rescan is now pinned at the btleplug layer: a
    CoreBluetooth identifier re-resolves through
    `retrievePeripheralsWithIdentifiers` (answered not-found, never a scan),
    the adapter keeps known peripherals across disconnects, and WinRT
    reopens by address. New tests in `vendor/btleplug`
    (`an_unknown_identifier_resolves_without_a_scan`,
    `a_disconnect_of_an_unknown_peripheral_sends_no_event`), the WinRT
    model (`a_listed_address_reopens_without_a_scan`), the backend identity
    round-trip (`f127_a_listed_identity_resolves_without_a_scan`), and an
    adapter-only hardware test
    (`cargo test -p ubm-desktop --test reconnect_without_rescan -- --ignored`).
  - Disconnect goes straight to the radio (the extra pre-disconnect
    `is_connected()` query is removed), and a disconnect whose radio answer
    is "device gone" (BlueZ `UnknownObject`/`DoesNotExist`) reports the
    link released — never stuck in `Disconnecting` — while transport
    failures still keep it pending.
  - BlueZ: `security.state().pairingPossible` is the legacy constant `true`
    again; the custom-ceremony refusal is
    `bluez.security.pair.custom-ceremony`; an unreported address type maps
    to `random`; no reported UUIDs leave `serviceUuids` absent;
    `Device.Connect` tolerates `AlreadyConnected`; and `Disconnect` waits
    up to 1 s for `Connected=false` (a gone object confirms, a timeout
    stays pending).
  - WinRT: a without-response descriptor write fails closed with
    `gatt.write-failed`/`winrt.gatt.write-descriptor`; an unknown adapter
    id is `adapter.unavailable`; connect on a never-observed peer is
    `peer.not-found` on every desktop backend — renamed from the legacy
    `connection.not-found` (`winrt.connect.peer`,
    `direct-gatt.connect.peer`, `bluez.connect` keep their shape) — and a
    peer refusal for lack of authentication, authorization or encryption
    is `platform.security`: the vendored radio now reads the ATT error
    byte from `GattReadResult`/`GattWriteResult` (`attError` in the
    platform detail, patch 20), so Windows matches every other host.
    Other ATT errors keep `gatt.read-failed`/`gatt.write-failed` with the
    byte in the detail; a without-response or CCCD write, whose WinRT call
    returns no result object, keeps its GATT code.
  - CoreBluetooth: the readiness watch drops the oldest observation on
    overflow, reprobes every 100 ms while unready (bounded by the caller's
    deadline/signal), and buffers pre-probe reports for replay; the
    pre-admission abort carries the bare operation id; the when-available
    intent refusal is `direct-gatt.connect.when-available`; GATT verbs
    serialize per connection with fail-fast `lifecycle.invalid-state`; RSSI
    keeps its integer-dBm precision limits; unprovided observation fields
    are `unavailable` with a backend-scoped device (provenance was already
    `not-provided`); empty maximum-write-length connection ids are
    `argument.invalid`; backend-event and adapter-state streams keep their
    64/64KiB/1 and 16/16KiB/1 quotas; and the readiness capability row no
    longer claims btleplug provides it on unpatched builds.

- Desktop reconnects, disconnects and notification delivery match 4.x
  again.
  - **Reconnect after a disconnect.** On macOS and Windows, a reconnect
    by id after a disconnect answered `peer.not-found` until a new scan.
    Now disconnected peripherals stay known, and one the OS forgot is
    resolved again (CoreBluetooth `retrievePeripheralsWithIdentifiers`,
    WinRT by address; vendored patch 19). A local disconnect that races
    a remote loss reports the link released.
  - **Pending work fails at disconnect.** On macOS, a discovery,
    descriptor read or descriptor write in flight at a disconnect or
    power-off hung. It now fails at once, as 4.x's `failPendingForDevice`
    did. On Linux, a disconnect during a connect's service discovery ends
    it at once instead of timing out after 5 s.
  - **First values after subscribing.** The notification stream opens
    before notifications are enabled, so values the device sends
    immediately are no longer lost.
  - **Values before a disconnect.** Values already received when a
    disconnect or service change arrives are delivered before the
    invalidation, or counted as loss; they are never dropped silently.
  - **Ingress drops reported per subscription.** A value the desktop
    intake refuses is reported to its own subscription as upstream loss
    under the consumer's overflow policy: `error` ends the stream with an
    `overflow` terminal, a lossy policy counts it
    (`DesktopCentral::subscribe_with_policy`, `consumer_counters`). The
    total is `ResourceCounters::ingress_notification_drops`.

- Desktop scans, pairing and error identity match 4.x again.
  - **Every sighting is reported.** Before, only "discovered" and
    "updated" btleplug events became observations:
    - on Linux, a device BlueZ already knew (paired, or seen by an
      earlier scan) was never reported again;
    - on macOS, a peripheral without a name was reported once and never
      again.

    Now every sighting is reported (vendored patch 17):
    - CoreBluetooth: every advertisement;
    - WinRT: every received event;
    - BlueZ: discovery, once per device property-change signal
      (including a name- or alias-only change; vendored patch 18), and
      every known device when a scan starts, as 4.x did.

    Tauri also keeps its 4.x cadence, re-reading every known peripheral
    every 2 s during a scan.

  - **Observations belong to their scan.** Sightings are observations only
    while a scan runs. Each carries its scan's operation id and its age
    (`take_scan_observation`). A new scan never receives what an earlier
    scan, or the time before any scan, saw.
  - **Each observation carries its own data.** CoreBluetooth and WinRT
    observations carry that advertisement's own payload, not the merged
    per-peripheral state. BlueZ observations are the OS's merged device
    state and are labelled `device-state` (`extras.source`). Unreadable
    sightings and properties are counted and reported instead of turning
    into empty records. A malformed WinRT service-data section no longer
    panics.
  - **Pairing without a deadline waits.** Desktop pairing and
    cancel-pairing without a caller deadline no longer time out after
    120 s; a cancel still ends them.
  - **Error identity leftovers.** Unsubscribe failures carry the platform
    detail. Every BlueZ failure, including non-D-Bus ones, is
    `platform.failure` with a `bluez-dbus` detail (`org.bluez.Error.Failed`
    when D-Bus gave none), as in 4.x.

- Rust core connects, errors, CoreBluetooth reads and notification
  delivery match 4.x again.
  - **A connect without a deadline waits.** A connect without a caller
    deadline no longer times out after 120 s. As in 4.x (a pending
    CoreBluetooth connect, Android `autoConnect`,
    `connection:when-available`), it waits until the OS answers, and a
    cancel ends it.
  - **Platform error identity.** Errors carry the platform's answer as
    typed fields again (`DesktopError::platform()`: domain, code, message,
    metadata):
    - CoreBluetooth: the `NSError` code;
    - WinRT: `gatt-status` with `gattStatus`, or `hresult` with `hresult`;
    - BlueZ: the D-Bus error name. BlueZ D-Bus failures are
      `platform.failure` again, as in 4.x.

    CoreBluetooth read, write and notification errors, which the vendored
    btleplug dropped, are now answered instead of waiting for a deadline.
    Failed descriptor operations no longer panic.

  - **CoreBluetooth reads cannot return a notification.** macOS refuses a
    read on a notifying characteristic (413), a second read while one is
    pending (414) and a subscribe while a read is pending (415), as the
    4.x addon did. Before, a notification could come back as the read's
    value (vendored patches 14 and 15).
  - **Immediate security, readiness and scan-end reports.** On desktop,
    security changes, write readiness and a scan the OS ended now wake the
    host immediately, as 4.x callbacks did. Before, they waited for the
    5–10 ms polling interval (finding 118).
  - **WinRT scans use the OS service filter.** WinRT scans put the
    caller's service UUIDs on the OS advertisement filter again, as 4.x
    did (vendored patch 16). Software matching remains the final check
    (finding 117).
  - **No value lost at invalidation.** Values already queued for a
    subscription when its link is lost, its services change or the adapter
    resets are delivered, in order, before the subscription ends.

- Rust core discovery, adapter, BlueZ and test-radio behaviour matches 4.x
  again.
  - **No product limits on GATT databases, subscriptions, links or
    remembered peers.** The core stopped at 128 GATT entries, 32
    subscriptions, 16 links and 16 remembered peers, and a larger database
    was cut short while `discover()` still succeeded. The bounds are now
    protocol limits no real device reaches: 65535 entries and subscriptions
    per peer database, 3840 links, and 65536 remembered peers. Path
    registration is linear, so a full-size database registers in about a
    second. A database registers whole or the discovery fails. A malformed OS
    UUID fails it with `protocol.malformed` (`discovery.snapshot.uuid`), and
    a database past the ATT handle space with `capability.limited`
    (`discovery.database-bound`), as 4.x failed. The `skipped` field is gone
    from `DiscoveryReport`, the mobile `gatt.discover` wire and the Tauri
    `gatt.discover` response.
  - **Discovery order.** Desktop snapshots list services, characteristics
    and descriptors in discovery (handle) order again, not UUID order.
    Occurrences are unchanged.
  - **Adapter reads survive a reset.** An adapter-state or authorization read
    racing an adapter reset answers the post-transition state instead of
    `operation.reset` (desktop) or `backend.reset` (Tauri). A Tauri request
    that arrives after the reset is still refused `backend.reset`.
  - **BlueZ.** When BlueZ does not report the MTU, writes without response
    are no longer refused above 20 bytes; BlueZ answers them, as in 4.x. A
    subscribe to a characteristic that declares neither notify nor indicate
    goes to `StartNotify`, and BlueZ answers, as in 4.x. CoreBluetooth and
    WinRT keep their 4.x property check.
  - **No admission or buffering quota below 4.x.** One lease is no longer
    limited to 8 live operations across all its links. 4.x queued 8 per
    connection with no per-owner cap, so one app driving three devices
    with 9 requests each was refused. A subscription is no longer limited
    to 8 consumers. The core's own buffers no longer drop values before
    the caller's stream would:
    - each consumer's subscription buffer was 64 values / 8 KiB;
    - the scan-observation queue was 256;
    - the desktop notification ingress was 256;
    - btleplug's event broadcasts held 16 (vendored patch 13).
      They now hold up to the public stream maximum (65,536 items), with
      4,096 slots for the broadcasts, and loss past that is still reported.
  - **Test radio.** `FakeRadio` answers `AlreadyPaired` when asked to pair a
    peer that is already bonded, as a real OS does.

- Rust core write and scan behaviour matches 4.x again.
  - **Long writes (Windows, Linux).** On WinRT (Node, Electron main,
    Tauri-Windows), a with-response characteristic write or a descriptor
    write now accepts a whole attribute value (512 bytes) and Windows
    performs the long write, as the 4.x WinRT addon and Tauri 4.x did.
    Before, anything longer than MTU − 3 failed `bytes.too-large`.
    Writes without response stay bounded by one ATT payload. The per-write
    ceiling on every desktop host is now the ATT attribute-value maximum, 512. It was 509, which clipped the 512-byte long writes that BlueZ and
    CoreBluetooth report. Radios that perform long writes themselves
    declare it with `WriteLimits::os_long_write`. `ATT_MAX_ATTRIBUTE_VALUE`
    and `ATT_DEFAULT_LE_MTU` are public in `ubm-desktop`.
  - **The OS answers reads and writes.** The core no longer refuses a
    characteristic or descriptor read or write because of its property
    flags (`gatt.property-not-supported`). As on every 4.x host, the OS
    answers, so peripherals that advertise wrong flags work again. The
    public GATT layer still resolves the write mode from the flags, as in
    4.x. Subscribe still requires notify or indicate.
    `WriteMode::required_property` is removed from `ubm-core`.
  - **WinRT scans passively.** The vendored btleplug no longer forces
    active scanning with extended advertisements. Scans use the 4.x
    defaults: passive, no extended advertisements, and no scan requests
    (vendored patch 11).
  - **BlueZ narrows discovery by name.** A name-prefix filter is sent to
    BlueZ as the `SetDiscoveryFilter` `Pattern` again, as 4.x did
    (`ScanFilterSpec.name_prefix`, `DesktopCentral::start_scan_matching`,
    vendored patch 12). Name matching in software is unchanged.

- Desktop (Node / Electron main): the 4.x adapter behaviour is back on the
  Rust path.
  - **Adapter loss tears down.** An adapter loss is a power-off, resetting,
    unsupported, a revoked authorization, a removed adapter or a restarted
    bluetoothd. In-flight operations settle `operation.reset`. Scans and
    subscriptions end `source-failed`. Links are released: CoreBluetooth and
    WinRT emit `connection-state-changed` with reason `adapter`, and BlueZ
    ends them silently. The backend generation advances, so earlier
    connection, database and subscription handles are stale (peer handles stay
    usable; see the adapter-loss entry above). CoreBluetooth and BlueZ emit
    `backend-restarted`.
    Each OS keeps its 4.x sequence. Previously only an `adapter-state` event
    was emitted, and nothing was released.
  - **Admission errors are raised again** before any radio effect:
    `permission.denied` / `restricted` / `not-determined`,
    `adapter.unavailable`, `adapter.powered-off` and `adapter.resetting`.
    CoreBluetooth and WinRT use their 4.x ordering; BlueZ refuses only on
    lifecycle, as 4.x did.
  - **CoreBluetooth first-state wait.** CoreBluetooth waits up to 10 s for a
    usable first state, then fails with `capability.unavailable` /
    `adapter-initialization-timed-out`.
  - **Adapter state vocabulary.** Adapter power reports `resetting`.
    `unsupported` reports availability `unsupported`. `unauthorized` reports
    authorization `denied`.
  - **`duplicatePolicy: 'merged'` is accepted again.** It is the default of
    `scanForServices` / `scanUntil`. The policy reaches the OS duplicate
    filter.
  - **Maximum write length before discovery.**
    `connection.maximumWriteLength(mode)` answers without a discovered
    database, as 4.x did.
  - **Unsupported rows keep their reasons.** CoreBluetooth
    `connection:request-mtu` (`corebluetooth-auto-negotiated-mtu`),
    `connection:effective-mtu` and `connection:phy` are registered
    `unsupported` with their 4.x limitations. BlueZ
    `security:pairing-generation` is always registered: with the privilege
    explanation when no host controller is supplied, and with the
    adapter-wide blast radius when one is.
  - **WinRT adapters.** Any listed adapter can be selected again. Each
    adapter reports its process `deployment` (`packaged` / `unpackaged`) in
    its limitations and in the backend diagnostics.
  - **N-API additions** (`UbmCentral`): `takeAdapterResetEvent`,
    `adapterStatus`, `awaitUsableAdapter`, `connectionMaximumWriteLength`,
    `vendoredBtleplugPatches`, `startScan({ duplicatePolicy })`, the
    `adapter-lost` lifecycle kind and the `adapter-reset` invalidation
    cause. The applied btleplug patch list is reported as
    `diagnostics.btleplugPatches`.

- Desktop Rust core (`ubm-desktop`, used by Node, Electron main and Tauri):
  the adapter and GATT behaviour of the legacy desktop backends.
  - **Adapter loss.** On the production radio, a usable adapter that
    reports powered-off, resetting, unsupported or unauthorized, a denied or
    restricted authorization, a BlueZ `org.bluez` owner change or `Adapter1`
    removal, or a Windows adapter removal tears down everything live:
    in-flight operations answer `operation.reset`, the owned scan ends
    aborted, every link publishes the new `LifecycleKind::AdapterLost`, polls
    answer the new `InvalidationCause::AdapterReset`, OS releases are
    requested (failures named), and the attachment moves to new backend and
    adapter generations. One `AdapterResetEvent { cause, previous, current,
cancelled_operations, ended_scan, released_links, ended_subscriptions,
release_failures }` reports it (`adapter_reset_events()`,
    `CentralSignal::AdapterReset`). `attachment()` now returns an owned
    `AttachmentTuple`, which changes after each reset.
  - **Admission.** Operations refuse before any effect with
    `adapter.powered-off`, `adapter.resetting`, `adapter.unavailable`,
    `permission.denied`, `permission.restricted` or
    `permission.not-determined` (domain `adapter`, commit `not-dispatched`),
    in the legacy CoreBluetooth or WinRT order (`AdmissionPolicy`); BlueZ has
    no adapter gate, as before. `adapter_status()` reports power,
    authorization, availability and whether a loss is in effect;
    `AdapterPowerState` gains `resetting`, `unsupported`, `unauthorized`.
  - **macOS first state.** `open_btleplug` waits at most 10 s for a usable
    CoreBluetooth adapter (`await_usable_adapter`), else fails
    `capability.unavailable` / `adapter-initialization-timed-out`.
  - **Scans.** `start_scan_with(owner, uuids, ScanDuplicatePolicy, ctl)`:
    the duplicate policy reaches CoreBluetooth `AllowDuplicatesKey` and
    BlueZ `DuplicateData`; BlueZ scans LE only; a CoreBluetooth scan
    requested while not powered on fails instead of silently not scanning.
  - **Connection write limit.** `connection_maximum_write_length(peer,
lease, with_response, ctl)` answers without discovery.
  - **Repeated GATT UUIDs.** Same-UUID services, characteristics and
    descriptors stay distinct on macOS, Windows and Linux, and same-UUID
    instances can be subscribed side by side (the former "ambiguous
    routing" refusal is gone).
  - **Windows discovery and adapters.** Discovery is uncached with no
    cached fallback and names a failing `GattCommunicationStatus`; a
    non-default adapter opens by id; `AdapterListing.deployment` /
    `host_deployment()` report `packaged` / `unpackaged`.
  - **Broadcast loss is reported.** Notifications the OS broadcast lost
    before a subscription read them end an `error`-policy stream with an
    `overflow` terminal counting them (`notification_loss()` reports the
    count), and lost adapter events are counted
    (`ResourceCounters::radio_events_lost`). btleplug had dropped both
    silently.
  - **Vendored btleplug patches 6–10 are required**
    (`attribute-instances`, `central-state-detail`, `scan-policy`,
    `winrt-uncached-discovery`, `winrt-cccd-mode`, `winrt-adapter-by-id`,
    `stream-lag-reported`; `vendor/btleplug/UBM_PATCHES.md`): a build that
    links crates.io btleplug fails with the missing patch names instead of
    compiling a degraded radio. `vendored_btleplug_patches()` lists what is
    linked.
  - **Test radio.** `FakeRadio::set_peers` scripts the known-peripheral
    listing (`peers()`), `set_os_policy` the adapter gate and teardown,
    `scan_filters()` shows the applied scan filter.
    **React Native and Expo on the Rust route.**

- React Native restoration: restored peers are adopted once per process
  again. Legacy cleared the process radio's restoration identifiers on the
  first successful adoption. The Rust route had consumed them per manager
  only, so a second manager could adopt the same peer again. The Rust owner
  now hands each restored peer to one adopter per process
  (`peers.claim-restored`, docs/MOBILE_RUST_WIRE.md). A later manager's
  adoption replays the adapter record only, as a later legacy attachment did.
- React Native resource counters describe the manager again. They count what
  the manager's own session lease holds, so several managers no longer see
  each other's scans, links and subscriptions, and each returns to baseline
  on its own. The whole owner's totals are reported separately, as `process`,
  in the Expo host-services `counters()` record.
- React Native `connect({ preferredPhy })` is honoured on Android. The link
  is established on the requested LE PHYs (`connectGatt` with a PHY mask, API
  26+). Legacy ignored the option; the first Rust route refused it. The
  request is refused with `capability.unsupported` before any effect in three
  cases: on Apple (unchanged, CoreBluetooth has no PHY control), with
  `intent: 'when-available'` (Android ignores the connect PHY with
  `autoConnect`), and when the link is already established.
- React Native Android advertisements carry `appearance` and `rawRecord`
  again. The radio produced both, but the Rust route dropped them.
  CoreBluetooth reports neither, as before.
- React Native `diagnostics.traceMaximumRecords` / `traceMaximumBytes` are
  honoured again. Every operation the manager sends to the Rust owner is
  recorded, payload-free, in the bounded trace (defaults 256 records /
  512 KiB), so `traceDocument()` and the public diagnostics snapshot are no
  longer empty. Out-of-range bounds are refused before any session opens.
- Expo `restoration.claim()` works again for apps that configure restoration
  only in their Info.plist (`UnifiedBleProtocolRestorationId` /
  `UnifiedBleProtocolRestorationGeneration`), without a JS `restoration`
  option. The TurboModule's `restorationIdentity('{}')` answers the
  configured identity, or `null` when there is none.

- Desktop Rust provider:
  - Required service UUIDs from the public query reach the OS scan filter
    (`scanner.plan`).
  - Name-prefix, manufacturer and address filters match in software instead
    of failing with `capability.unsupported`.
  - Scan share/join works.
  - A without-response write reports `commitState: 'unknown'`, never
    `confirmed`.
  - `require-*` delivery modes are checked against the characteristic's
    properties, and WinRT carries them to the OS.
  - Values report the delivery the core observed.
  - Core errors keep their retryability and commit state across N-API (wire
    form `code|domain|operation|retryability|commit|detail`).
  - A dispatched write that may have committed stays non-retryable.
  - Shutdown release failures reach the cleanup record instead of being
    reported as released.
  - Aborts cancel exactly the in-flight core operation through a ticket.
  - A caller with no deadline gets the core's liveness backstops instead of
    a 49-day timeout.
- The legacy CoreBluetooth/WinRT native loaders name their ESM failure
  (`capability.unsupported` / `esm-legacy-boundary`) instead of masking it as
  an unavailable artifact.

- Tauri: the webview no longer sends its `performance.now()` deadline across
  the plugin boundary. A route with a caller deadline carries
  `payload.budgetMs` instead — the remaining budget in whole milliseconds,
  measured just before `invoke`, never negative, and absent when the caller
  gave no deadline — so the Rust side admits it against its own clock.
  Deadline expiry still routes `operation.cancel` for the exact target
  correlation.
- Electron: the renderer no longer sends its `performance.now()` deadline to
  main, whose clock has a different time origin. A route with a caller
  deadline carries `payload.budgetMs` (the remaining budget in whole
  milliseconds, measured just before send, never negative, absent without a
  deadline), and main admits it against its own monotonic clock at receipt,
  so main-side queueing counts against the budget and an expired budget times
  out with no effects. Main rejects an absolute renderer `deadline` as
  `protocol.malformed` instead of comparing it with its own clock. Renderer
  deadline expiry still routes `operation.cancel` for the exact correlation.
  The Electron renderer and the Tauri transport share one budget encoder.
- **Mobile session identity is process-monotonic; the host value-signal queue
  is genuinely bounded.** `ubm-mobile` session instance identity is now
  minted from a process-monotonic counter instead of an allocator address, so
  a dropped-and-recreated session can no longer reuse an old instance id
  (this is what made the Windows CI job fail). Host value signals now share
  one queued marker with a bounded batch drain, so the signal queue stays
  bounded however many value scopes go dirty at once. `counters.describe`'s
  `process.native` gains `connectSections`.

- React Native/Expo Android: writes no longer fail `capability.unavailable`
  before an MTU exchange (finding 80). The platform radio now answers the
  write limit per mode (`ReadWriteLimits`, docs/MOBILE_RUST_WIRE.md): Android
  accepts a with-response write up to 512 bytes, the ATT maximum attribute
  value, because its stack performs the long write, and bounds a
  without-response write by one ATT payload of the reported MTU, or of the
  ATT default MTU 23 before any exchange. A command larger than that is
  refused `bytes.too-large` before dispatch instead of being handed to the
  stack. iOS bounds each mode by CoreBluetooth's
  `maximumWriteValueLength(for:)`: with response up to 512 bytes, which
  CoreBluetooth long-writes, and without response by its own per-type limit
  rather than the with-response value.
- React Native/Expo: no Rust mobile owner bound is below legacy React
  Native's any more (finding 108). The platform ingress queues for
  advertisements and control facts hold 512 each, up from 256, matching
  legacy's 512 records per binding. docs/MOBILE_RUST_WIRE.md lists every
  bound.
- React Native/Expo: radio failures carry legacy React Native's error
  identity again (finding 113). A GATT or other radio failure is
  `platform.failure`, and its platform detail is
  `{domain:'android', code:<native code such as 'writeFailed'>,
metadata:{androidGattStatus}}` on Android, or the `NSError` domain and code
  on iOS. An Android link loss (including GATT status 19) is
  `connection.lost` with code `connectionLost`. The owned CoreBluetooth
  radio's read/notify refusals keep `gatt.read-failed` and
  `gatt.subscribe-failed`. The Rust route had reported `gatt.*-failed` codes,
  with the status only as free text. The mobile wire error gains a validated
  `platform` object.
- React Native/Expo Android: several link-control and scan-option refusals
  match legacy React Native again (finding 139):
  - Scan `platform.phy` and `platform.reportDelayMs` are
    `capability.unsupported` (`scan.start.platform-options`) at the owner
    too. The provider already refused them.
  - A `request-phy` with no preference is `argument.invalid` in the
    `connection` domain.
  - A `request-mtu` below 23 goes to the platform, whose refusal is
    `platform.failure` with code `requestMtuFailed`.
  - Reading or requesting the PHY below API 26 is `capability.unsupported`
    before any radio call.
    Aborting a pairing still ends the caller's operation at once, and the
    Android bond ceremony and its pending record run to completion, as they
    did on 4.x.
- React Native/Expo Android: an operation in flight or queued when the link
  goes down (peer or app disconnect, a failed connect, a close timeout) fails
  `connection.lost` with code `connectionLost` and the disconnect's GATT
  status, as legacy did (finding 132). It had failed `platform.failure`. On
  iOS such an operation keeps its `NSError` identity (`platform.failure`), as
  legacy reported it.
- Expo: `background.*`, `association.associate` and `restoration.claim` report
  legacy Expo's error codes again (finding 133):
  - A foreground service that is not configured or not running is
    `capability.unavailable`.
  - A permission denial is `permission.denied`.
  - An invalid lease is `lifecycle.invalid-state`.
  - An invalid request is `argument.invalid`.
  - An unsupported platform is `capability.unsupported`.
  - Every companion-association failure is `capability.unavailable`.
  - Every restoration-claim failure is `capability.unavailable`.
    The native code (for example `foregroundServiceNotConfigured` or
    `associationCancelled`) is the error's `platform.code` under the `expo`
    domain.
- React Native/Expo: `security.pair`, `security.cancel-pairing` and the
  Expo companion chooser (`companion.associate`) wait for the user or the OS
  without a deadline when the caller gives none, as legacy did (finding 123).
  They no longer end at the 120 s liveness backstop. A caller deadline still
  applies, and a cancel still ends the wait.
- React Native/Expo: a `connection.connect` without a deadline, including
  `when-available`, waits until the OS answers or the caller cancels, as
  legacy Android `autoConnect` and pending CoreBluetooth connects did
  (finding 112). It no longer ends at the 120 s backstop.
- **Wire change: `op.cancel` is classified exactly again, as legacy's
  dispatch epoch did** (finding 109). Every invoke that names an operation
  carries a strictly increasing per-session `admission`, and `op.cancel`
  carries its target's. A cancel is `already-terminal` when its admission has
  been seen, and pre-admission (its invoke then fails `operation.aborted`
  with no effect) when it has not, so no amount of history can misclassify
  it. This replaces the bounded memory of finished ids and tombstones. The
  React Native provider stamps admissions in send order, answers a cancel of
  a settled operation locally, and refuses an operation cancelled before it
  was sent without reaching the owner.
- React Native/Expo: a control record the Rust owner could not queue is no
  longer only a diagnostic (findings 104/105). The new `session.reconcile`
  wire op answers every fact a control record carries: held and ended links
  with their reasons and generations, database changes, each consumer's
  stream state with its terminal reason and drop counts, security reports,
  restored peers and scan membership. After an
  `ingress-drop{class:"control"}` the provider turns each lost record into the
  transition it would have caused: link end with its reason, loss of a link
  the owner reconnected under a new generation, database invalidation, stream
  end, `bond-security-changed`, `restoration-received`, or scan end.
- React Native/Expo: a `db-changed` record from the Rust owner now
  invalidates the databases it names. The record carries the generation the
  change invalidated, and the provider had been skipping exactly those
  databases, so their handles stayed usable after a service change.
- React Native/Expo Android: a connected-device foreground-service lease
  belongs to the `UnifiedBleRustCore` module instance again, as it did on the
  legacy route (finding 87). Destroying or recreating a manager keeps the
  foreground service, and the lease handle's `release()` still works after
  its manager is destroyed: it releases through a short-lived session of the
  same module. Any manager of the same module may update or release the
  lease, and module invalidation (a React reload or teardown) releases it.
  `session.dispose` no longer stops the service.
- The React Native API report no longer exposes the internal
  `CoreTraceRecorder` class: `ReactNativeRustCoreProviderOptions.trace` is
  typed with the public `CoreTraceSink` interface, which has one
  `record(input)` method (finding 76).

- Tauri behaves as Tauri 4.x did again on the shared Rust core (findings 43,
  57, 58, 60, 90, 114, 116).
  - **Platform error identity.** A failure the OS answered carries the OS's own
    identity in `BleError.platform`, the same as on the Node desktop path:
    CoreBluetooth `{domain: 'corebluetooth', code}`, WinRT
    `{domain: 'winrt', code, metadata: {hresult, gattStatus}}`, BlueZ
    `{domain: 'bluez-dbus', code}`. A failure without OS detail keeps the 4.x
    `{domain: 'btleplug', code: 'native-error'}` shape. The webview transport
    accepts only strings, finite numbers and booleans in `platform.metadata`
    and refuses anything else as `protocol.malformed`.
  - **Adapter loss.** When the adapter powers off, resets, becomes unsupported
    or unauthorized, or is removed, every link ends `connected → lost` with
    cause `adapter-loss`. Notification streams and the scan end
    `source-failed`, and the attachment moves to a new generation. Requests on
    the previous attachment fail `backend.reset` before any native call.
    Releases, and `manager.destroy()`, still settle what the webview holds.
  - **One radio.** The plugin no longer opens a second btleplug manager (a
    second `CBCentralManager` on macOS) for attachment identity and
    `adapter.state`. Both come from the shared central, which applies its
    adapter selection: several adapters and no name is `adapter.ambiguous`.
  - **Adapter state.** `adapter.state` reports `resetting` and `unsupported`
    power. It reports authorization as the OS reported it, and reports a
    removed adapter as `unavailable`. Admission refusals
    (`adapter.powered-off`, `adapter.resetting`, `permission.*`) reach the
    webview unchanged.
  - **Maximum write length.** `connection.maximum-write-length` is the core's
    answer for the requested write mode, so a write of the reported size is
    never refused (512 with response on Windows and Linux). Before, the plugin
    reported `mtu - 3` for every mode.
  - **Orphan cleanup retries.** The plugin retries a resource it admitted for
    a window that went away, such as a cancelled scan, connect or subscribe
    that the core still carried out, when its release fails. It retries
    automatically on the 4.x schedule: 8 attempts, 100 ms doubling to 5 s. The
    window's release reports it as `tauri.quarantine.exhausted` once all 8
    attempts were refused. Retries continue after the window closes, so an
    orphan does not stay up.

- **`unified-ble-manager/electron/main` exports the BlueZ backend
  identity** (`bluezCompatibility`, `BLUEZ_BACKEND_ID`, …) and the rest of
  `node/bluez`, as it already did for CoreBluetooth and WinRT. The Electron
  example no longer reaches into `node/bluez` for it
  (`etc/api/electron-main.api.md` regenerated).

### Fixed

- **Desktop scan duplicate acknowledgements are bounded and attachment-scoped.**
  A long-lived central retains the exact terminal outcome for its newest 256
  completed scans, matching the public scan-state bound. A recent duplicate
  cancel still suppresses onto that terminal; a ticket from the same attachment
  after that acknowledgement window reports `lifecycle.invalid-state` with an
  explicit expiry detail, and cancellation of a ticket from another central
  reports `ownership.denied` (`scan.stop` remains the idempotent `NotActive`
  cleanup result). Opaque operation ids now include their immutable
  attachment scope, so independently opened centrals cannot alias local
  counters.

- **Background continuation serializes ownership without racing its claim
  (Android).** An executing native wake and `continuation.claim()` now reserve
  the same session before either performs radio I/O. A concurrent operation
  receives a typed lifecycle refusal and leaves the session intact; it cannot
  dispose a session that the wake later reports as completed. The Companion
  service admits callbacks to one serial worker and returns promptly to CDM;
  teardown closes admission but lets work already admitted finish. Focused
  regressions prove the two reservation orders and admitted-work teardown.
  The wake also converts public 1-based GATT occurrences to the Rust mobile
  wire's 0-based paths. Claims validate consumer indices against a
  `consumerCount` captured from the exact native session and returned
  atomically with its backlog, including when a build-time manifest leaves the
  JavaScript manager option absent. On a Samsung SM-A376U1, a real Polar H10
  absent-to-present transition woke the app with no JavaScript, connected,
  discovered and subscribed in about 1.6 seconds; Logcat contained no app ANR
  during this run, the service recorded queued bytes before JavaScript opened,
  and the later claim drained 30 heart-rate values with zero dropped items,
  dropped bytes or control loss.

- **Background continuation revalidates liveness and pins selector identity
  (Android).** A repeated presence wake now reads the Rust owner's
  `session.reconcile` snapshot before treating an existing continuation as
  complete: it remains idempotent only with a current link and live routes;
  an ended link or route reconnects and receives fresh consumer ordinals.
  A different same-count, reordered, or occurrence-changed declaration is
  refused while the old session is claimable. Claims carry the immutable
  selector list for their numeric consumers, so a later declaration cannot
  relabel an older backlog.

- **Background continuation claims have an atomic native cutoff (Android).**
  The Rust outbox now seals under the same lock as data admission before Kotlin
  drains it. Records admitted before the cutoff are returned exactly once;
  later observed intake is reported through `afterCutoffLoss` rather than
  disappearing between the final drain and cleanup. Kotlin validates the
  required drain shape before authorizing cleanup, returns only validated
  prefixes on a malformed boundary, and keeps failed cleanup owned for retry.
  An unreadable acknowledgement returns the decoded backlog with explicit
  disposal uncertainty; a retained empty native receipt lets the next claim
  confirm cleanup without delivering those values again.

- **TV staging uses a portable archive and executable Python shim.** The TV
  staging copy writes a temporary tar archive before extracting it, avoiding a
  macOS system-tar pipe failure while retaining Git Bash compatibility. Windows
  test shims are created in Git Bash's executable temp location rather than a
  host temp directory that Git Bash can resolve but cannot execute from.

- **Android prebuild verification uses the NDK's ELF symbol reader.** The
  canonical builder no longer asks the host `nm` to parse an Android `.so`;
  macOS `nm` accepted no GNU `-D` mode and falsely reported that present JNI
  exports were missing. Both release ABIs now verify with the pinned NDK's
  `llvm-nm`, the same toolchain that links them. Both Rust builders resolve
  `rustc` from the pinned rustup toolchain rather than accepting an older host
  `rustc` earlier on `PATH`. The Android and desktop N-API builders also read
  their completed libraries from `CARGO_TARGET_DIR` when Cargo is directed to
  an isolated target tree, instead of copying a stale default-target binary.

- **Desktop slow-drain acceptance has a deterministic ingress barrier.** The
  desktop Rust-core TCK waits until staged notifications reach the bounded
  stream before reading the slow-drain window. The test now exposes overflow
  accounting under the same deterministic synthetic workload on Linux and
  Windows instead of racing an independent notification pump.

- **Desktop adapter resets use the core's causal power and sequence.** A reset
  applies its factual power state at the new generation and discards queued
  state observations at or before its causal sequence. Delayed-state and rapid
  off/on regressions pin the ordering, replacing the prior status-read
  inference.

- **Cargo license output resolves legacy slash declarations precisely.** Slash
  alternatives are normalized to SPDX `OR`; `btleplug@0.12.0` is resolved from
  its reviewed, digest-bound vendored license file as
  `BSD-3-Clause AND (MIT OR Apache-2.0)`. The regenerated SBOM and third-party
  notice now have zero unresolved Cargo entries.

- **SIM0001's configured one-second advertising cadence has physical HCI
  evidence.** A separate Linux controller captured raw extended advertising
  reports: observed consecutive gaps include 992 ms, 1003 ms and 1007 ms, and
  longer gaps were whole-second multiples. This establishes nominal cadence;
  it does not claim RF reception of every advertisement.

- **The native refresh consumers work on Windows.** `ensure-native.js` spawned
  `pnpm` with no shell. Windows runs pnpm as `pnpm.cmd`, which only executes
  through `cmd.exe`, so every consumer that refreshes native artifacts through
  it — `hosts.sh up`, the TV build, the check-only switch — failed there and
  could never report why. It now spawns through a shell, the same command on
  every platform. Two developer scripts had the same class of bug: they passed
  a shell path inside a quoted `node -e` program, which Git Bash leaves in MSYS
  form (`/c/...`) because it only rewrites path-shaped arguments, and they
  hardcoded `python3` where Windows names it `python`. Both are fixed portably
  rather than with a platform branch, and guards pin the shapes so they cannot
  come back.

- **A background wake no longer abandons a session it could not release.**
  `claimContinuation` read the dispose result and cleared the session id
  regardless, so a release that reported failures left the connection or
  subscriptions with no owner and no retry — the most expensive shape of bug,
  because nothing reported it. The claim now keeps the session when the release
  reports failures, returns the reason as `disposeFailure`, and the next claim
  retries. (One path reports less: when a queued batch is itself unparseable the
  claim fails closed with `protocol.malformed` before a backlog is built, so the
  session is kept but the reason arrives as that error rather than as
  `disposeFailure`.) An incomplete drain, or a full batch cap with more still queued,
  likewise keeps the unread tail instead of discarding it. Applications must
  claim again while `disposed` is false.

- **Presence rejections are recorded instead of returning silently.** An
  appearance from an unassociated device is recorded as `association.unknown`,
  and an appearance outside a `peerId`-scoped order is recorded as a scoped
  skip; both reach `continuationStatus().lastWake`. Previously both returned
  with no record at all, so `lastWake` stayed null forever and an operator
  could not tell "never woken" from "woken and refused".

- **Apple's continuation status tells the truth about what will execute.** It
  reported peer id null, zero malformed declarations and no last wake, and
  echoed the declared strategy unvalidated, so a declared native standing order
  looked accepted on Apple when nothing would run it. It now validates the
  declaration by the same rules as Android, reports what was actually declared,
  and carries a `detail` saying the strategy is not implemented in this
  release. `malformedDeclarations` is persisted per distinct declaration rather
  than counted per read.

- **The wake no longer holds its lock across radio I/O.** Execution held a
  single lock across a connect budget and up to sixty-four subscribes, so a
  second appearance, a disappearance or teardown queued behind it. The lock now
  guards state only.

- **The repository says one thing about its own license.** The dual
  arrangement itself is unchanged and was already stated correctly in `NOTICE`,
  the README, `CONTRIBUTING.md` and every crate manifest: material retained
  from the Apache-only baseline stays under Apache-2.0, and new 5.0 work is
  under the UBM Source Available License 1.0. Several documents had not caught
  up and still called the package "open-source", which its own license text
  explicitly says it is not — `GOVERNANCE.md`, `SUPPORT.md`, the 4.0
  implementation plan and the threat model now say source-available and point
  at `NOTICE`, and `RELEASE.md`'s release invariant names the metadata and the
  documents it has to reconcile instead of a single `LICENSE`. Dated records
  (the 4.0 roadmap banner, the governance ADR) keep their original wording
  behind a note saying it is history, because a decision taken in 2026-07 is
  not rewritten by a license adopted later. The license's own "qualifying
  open-source project" term is part of the grant and is untouched, and pinned
  by test.

- **The examples refuse a dev-server port that belongs to another project.** A
  React Native app pointed at a Metro serving a different project loads that
  project's bundle, registers no callable JavaScript modules, and then throws on
  every native call — thousands per second, without bound, growing about 20 MB a
  second until the machine runs out of memory. Nothing in that sequence names
  the cause: it reads as a hang. The bare example's `start`, `ios` and `android`
  scripts, and `build-tv.sh metro`, now check who holds the port first and stop
  with the holder's pid, working directory and command. Where the platform
  offers no way to identify the holder the guard says exactly that and lets the
  run continue, rather than refusing something it cannot judge. `example-expo`
  is unchanged: Expo already detects a busy port and offers another.

- **The repository's license file is the UBM Source Available License.** The
  top-level `LICENSE` used to be the full Apache-2.0 text, which GitHub, npm
  and most tooling read as the repository's license. It is now the UBM Source
  Available License 1.0, identical to `LICENSE-UBM-SOURCE-AVAILABLE-1.0.md` and
  pinned so the two cannot drift. Material retained from the Apache-only
  baseline is still under its Apache-2.0 grant — that grant cannot be withdrawn,
  and Apache-2.0 requires recipients to receive a copy of it — so the Apache text
  moves to `LICENSES/Apache-2.0.txt`, still shipped in the package, where it
  cannot be mistaken for the repository's license. `NOTICE`, the README,
  `CONTRIBUTING.md`, `RELEASE.md` and the packaging guide say so. The H10
  simulator declared `license = "MIT"`; as new 5.0 material it is under the UBM
  license like every sibling crate, and like them it is never published.

- **The Windows CI leg was reproduced and fixed on a real Windows machine.**
  Earlier fixes had been reasoned from macOS and did not hold. On a Windows VM
  with CI's own Node and Python, seven causes surfaced, most invisible from
  macOS: Git Bash mounts the Windows temp directory on `/tmp` and resets
  `TEMP`/`TMP`, so the TV stage guard rejected every legitimate stage; Git for
  Windows ships no `rsync`; `python3` there is a zero-byte Microsoft Store stub;
  there is no `lsof`; and a teardown `EBUSY` was replacing the real assertion
  failures. The stage now syncs with `find` and `tar`, the interpreter is kept
  only if it actually runs, port probes go over TCP, and the port guard's honest
  "cannot determine" answer is what the Windows tests expect.

- **The desktop adapter watch no longer reports a stale snapshot during a
  reset.** Powering the adapter off queues an adapter event and a core reset
  separately; when the reset drains first, the core supplies the reset's causal
  power and sequence. The new generation reports that fact, and queued states
  at or before that sequence cannot replay ahead of recovery.

- **The background wake reports the platform's answer, not its own timer.** The
  native wake connected `when-available`, which on Android parks a slow,
  fragile background connection — pointless, since presence only fires once the
  peer is already advertising — and gave GATT discovery no budget, so a
  ten-second client timer reported `platform.failure` while the core's own
  window was still open. The wake now connects `direct`, gives discovery a
  twenty-second budget, and every client timer waits for the core to answer
  first. (Not yet proven over the air.)

- **The packed Electron smoke offers the IPC protocol the renderer accepts.** A
  protocol bump from 3 to 4 had left the smoke test's stand-in main process
  offering 3; a test now ties that number to `IPC_PROTOCOL_VERSION`.

- **The Windows CI leg runs the shell-script behaviour tests it was skipping
  past.** Nine tests in three suites had never passed on `windows-latest`, each
  for its own reason and none of them a real platform limit: the TV stage guard
  only recognised `TMPDIR`, which Git Bash leaves unset, so a Windows temp dir
  was rejected as "not an absolute tmp dir"; one test clobbered `PATH` so
  narrowly that Node could no longer find `bash` itself, producing a failure
  with empty output; and the test harness's `pnpm` stub had no extension, which
  Windows cannot execute. The guard now accepts the host's real temp directory
  whatever names it (`TMPDIR`, `TEMP`, `TMP`, `/tmp`) and normalises the stage
  path for rsync, and the harness uses the platform path delimiter and a
  `pnpm.cmd` stub. Nothing is skipped: every behaviour is still proven on every
  platform that can run it.

- **The packed-tarball guard recognises the whole shipped surface.** The
  canonical pack verifier had never run to completion — earlier failures always
  aborted the job first — so three gaps in its allowlists only surfaced now:
  the `backends/desktop` sources, the five private runtime modules the
  Node/desktop entrypoints require, and `scripts/release/native-build-identity.js`
  plus the vendored crates, all of which ship deliberately. `npm pack` output is
  compared against the source tree again instead of failing on its own contents.

- **The sealed native build identity is regenerated after a Rust-source
  change.** A formatting-only `cargo fmt` stales the digest that
  `src/generated/native-build-identity.ts` seals, and `prepack` checks it, so
  four native build jobs failed on a change that altered no behaviour.
  `docs/NATIVE_ARTIFACTS.md` now names this file as the fourth thing a Rust
  change stales, explains that `native:refresh` does not write it, and gives
  the command that does.

- **Presence observation accepts the peer id the library hands out (Android).**
  `presence.observe` resolved the public peer id to the device address the
  platform requires; arming with the library's own identifier used to fail with
  "not a valid MAC address". A caller holding a platform address still works.

- **Adapter watch no longer reports a change that did not happen.** A
  platform re-announce that changes nothing observable — availability,
  authorization, power, `safeReason` — no longer reaches watchers as a
  transition, so a subscriber sees its initial snapshot followed only by real
  changes. Fixed on the desktop Rust core, React Native, BlueZ, WinRT and the
  legacy CoreBluetooth path (Web was already guarded); bookkeeping, backend
  `adapter-state` events and generation advances are unchanged. The TCK fact
  `adapter-watch-is-atomic-with-initial-snapshot` now runs on the BlueZ, WinRT
  and React Native legs too, not only CoreBluetooth, so this class cannot hide
  on one backend again. Found by CI on Linux and Windows while macOS passed.
- **`h10-sim` rejoins the test driver by itself (finding 224).** The simulator
  used to give up after three attempts, so a network interruption left it
  advertising but invisible to the driver, and every scenario against it timed
  out with no explanation. It now retries forever with bounded exponential
  backoff (1 s doubling to a 30 s cap, jittered), logging every attempt, its
  delay and the re-registration, and keeps its mode, faults, profile and run
  record across the rejoin.

- **Radio-failure connects report `caller-decides`.**
  `connection.failed` from a genuine radio failure (the WinRT helper, the
  desktop-core `classify_connect_failure`, and the unified-core non-contract
  fallback) now carries `caller-decides`/`retry-with-backoff`, matching the
  vocabulary. Terminal refusals (`already-owned`, `peer.not-found`,
  `ownership.denied`) stay `never`.
- **Web connect deadline docs corrected.** Behavior was already right
  (finding 161): a web connect deadline reports `connection.failed` with
  `DeadlineExpired`, and a retry on the same manager is a fresh attempt,
  never `connection.already-owned`. `docs/WEB.md` described the stale
  `operation.timed-out`/destroy-and-recreate shape; it now matches the code.
- **Tauri docs corrected: `connection:effective-mtu` is answered, not
  unsupported.** The dispatcher already answers `connection:effective-mtu`;
  `docs/TAURI.md` claimed it was `capability.unsupported`, which was stale.
- **Presence-appearance drain is exactly-once under concurrency (mobile).**
  The Android presence store snapshotted then wiped, so an appearance saved
  in between was lost silently; the drain now removes exactly the snapshotted
  keys under a lock shared with saves. Unparseable/foreign records are
  reported (log plus `PresenceRestoredStore.malformedRecordCount()`, surfaced
  once per drain in the process-host log) instead of cleared silently. A
  mobile-host test pins that a pre-reconnect-epoch notification never reaches
  the new subscription.
- **A lost link answers `connection.lost` on effective-MTU, RSSI and
  write-readiness reads.** Renderer abort and the budget-derived deadline are
  now forwarded on the Electron `connection.effective-mtu` route.
- **`h10-sim --compare` exits 0 only when the run is complete, not just
  passed.** `--compare` now exits 0 only when every check passed AND
  coverage is complete (`--allow-incomplete` opts into passed-alone);
  settled notifies are logged as `notify-settled`; the ECG stamp uses the
  last sample (was one sample late); the link-loss scenario pins
  `operation.disconnected`; join facts pin `physicalLinks === 1`; the
  capture test proves identity propagation instead of echoing the fake's
  own label.
- **H10 fidelity split: structural checks vs. OTA qualification.** In-process
  fingerprint tests are renamed to `structural_fidelity_has_no_mismatches_*`
  (partial, passed-only — they cannot measure over-the-air timing); a new
  `h10-sim --qualify-ota <real-capture> <sim-run-capture>` requires passed
  AND complete (`--allow-incomplete` never applies to it); the in-process
  path is pinned incomplete so it can never qualify as OTA.
- **Synthetic staging defaults to the live routing epoch.** A staged
  notification or loss with no epoch now delivers after a reconnect; an
  explicit stale epoch is still dropped. Real-radio forwarding is unchanged.
- **The React Native drain benchmark runs in plain node.** The
  wake-to-drain stall outside jest was a harness module-resolution artifact,
  not a pump race: `scripts/performance/w6-rn-throughput.js` measures
  in-process with the f01 loader pattern, and the jest leg runs the same
  implementation. Interleaving regression coverage lives in
  `rust-core-drain-no-lost-wake.test.js`. Deterministic-harness evidence only,
  never radio proof.

- **Desktop hosts report the effective ATT MTU (finding 217 follow-up).**
  `connection:effective-mtu` is `limited` (was `capability.unsupported`) on
  every desktop host: macOS derives it per link as
  `CBPeripheral.maximumWriteValueLength(for: .withResponse) + 3`
  (`corebluetooth-derived-effective-mtu`, same as iPhone/tvOS), Windows reads
  `GattSession.MaxPduSize` (`winrt-gattsession-max-pdu-size`), Linux reads
  the `org.bluez.GattCharacteristic1` MTU (`bluez-gatt-characteristic-mtu`;
  withheld links answer `capability.unavailable`). Wired as
  `DesktopCentral::read_effective_mtu` through N-API `readEffectiveMtu`, the
  Tauri `connection.effective-mtu` command, and the renderer IPC route;
  unpatched macOS or genuinely answerless platforms keep
  `capability.unsupported` with reason. Pinned by
  `crates/ubm-desktop/tests/effective_mtu.rs`, per-OS capability tests,
  NAPI/Tauri dispatch tests, and the desktop/Tauri suites.
- **iPhone and tvOS report the effective ATT MTU (finding 217).**
  `connection:effective-mtu` on the React Native Rust-core route is now
  `limited` (was `capability.unsupported`), derived per link as
  `CBPeripheral.maximumWriteValueLength(for: .withResponse) + 3`, so
  `connection.controls.effectiveMtu()` measures instead of refusing, naming
  the derivation `corebluetooth-derived-effective-mtu` alongside
  `live-radio-qualification-pending`. `connection:request-mtu` stays
  unsupported, and the legacy Apple reference route keeps its frozen
  refusal. Pinned by `rust-core-public-link-controls.test.js` and the parity
  update.
- **The H10 simulator matches real strap behaviour end to end (finding 216).**
  ECG timestamps default to the Polar epoch (`--clock unsynchronized`
  selects the old boot-relative clock); `drop-link` disconnects the
  simulator's tracked GATT clients plus any `--drop-link-allow` extras,
  reporting results per address (with no targets it reports `no simulator
clients`); the control port binds before the simulator reports ready,
  accepts lines up to 8 KiB, and enforces a 5 s auth deadline; `set-rates`
  applies atomically; PMD notification latency is measured off the event loop
  and now commits at indication send, so no frames go out before a
  `START`/`STOP` response and a central that vanishes mid-stream leaves
  nothing stuck; live profile edits are transactional with rollback on
  failure; and `compare` reports `complete` separately from `passed`. CI's
  sim-fixture freshness check now also ignores `osVersion`, not just
  `platform`. A new `--mode faithful|adversarial` run posture (default
  `faithful`) gates fault injection: the adversarial-only commands
  `delay-responses`, `flap-link`, `interrupt-next-subscribe`,
  `stale-callback` and `constrain-delivery` are refused loudly outside
  `--mode adversarial`, and `run-record` reports the run's seed/profile/mode
  and injected fault sequence with timestamps. The acceptance suite's
  link-loss scenario needs `--mode adversarial`.
- **Android presence cold start resumes the authorized known-peer workflow
  (issue #212).** A Companion Device Manager appearance with no live session
  used to stop at persistence: the process radio owner was never installed,
  so nothing was ingested or delivered. `UbmCompanionPresenceService` now
  installs the owner on appearance and delivers the associated peer as a
  `restored` record at once, persisting only what no owner takes for
  exactly-once drain at the next session open. There is still no scan, no
  connect, no resubscribe and no headless JS here: the app reconnects
  through the ordinary `when-available` connect.
- **Mobile scan admission honours cancellation and the deadline, and related
  races close.** A queued scan start now checks the caller's `AbortSignal`
  and deadline before it is admitted, and a dead radio start releases the OS
  scan instead of leaving it running. A concurrent scan start in the same
  session is refused `scan.already-active` instead of racing past a stale
  reservation. Same-peer connect staging is now serialized per operation,
  so a second connect for a peer already staging cannot interleave with its
  cleanup. The Rust-core drain response now carries a cumulative
  `controlLost` count, and a control loss triggers a prompt
  `session.reconcile` in the React Native provider instead of waiting for
  the in-band record to drain behind other data. Host signals are bounded
  to 1024 with per-scope latest-wins coalescing; lifecycle events are never
  coalesced, and overflow reconciles the same way.
- **Tauri `find()` no longer fails `stream.overflow` on a scan-start burst
  (finding 213).** Desktop scans re-report every known peripheral when they
  start. On Tauri the IPC path filtered after buffering, so about 50 nearby
  devices overflowed `find()`'s one-item budget within 100 ms. A drop-policy
  overflow notice is now loss accounting in `find()` and `scan.events`, as it
  already was in the in-process scan pump; only an error-policy overflow fails
  closed. `stream.overflow` is now `caller-decides`, which agrees with its
  retry-with-backoff recovery. Scan overflow errors name the drop policy, the
  dropped counts and the stream budget in `platform`.
- **Fixed the connect deadline abandoning the backend acquisition, and the
  retry it blocked (finding 194).** `UnifiedBleCore.connect` now cancels the
  backend acquisition through the `AbortSignal` in `ConnectionOptions` when
  the caller's deadline expires or the caller aborts. Before, it settled the
  caller while the backend call stayed in flight with a live `Connecting`
  claim. A deadline expiry with no link reports `connection.failed`
  (`caller-decides`, with the deadline fact in `platform`, finding 161); a
  caller abort stays `operation.aborted`. A retry for the same peer is
  admitted instead of refused `already-owned`: the React Native Rust-core
  provider supersedes an abandoned same-peer acquisition the same way the
  desktop provider does, and a newer connect for the same peer now supersedes
  this backend's in-flight acquisition (the abandoned caller ends
  `operation.aborted`, and arbitration admits the retry). A live established
  link still refuses with `connection.already-owned`, and BlueZ links join as
  before. Destroy no longer reports a settled acquisition as
  `connect.acquisition-pending`.
- **The Apple TV example build no longer ends with an empty library install.**
  Without a TTY, pnpm's modules-purge prompt emptied `node_modules` and exited
  0 without reinstalling, so the TV app later failed to resolve
  `unified-ble-manager/expo`. `build-tv.sh install` now confirms the purge up
  front and fails if the library is missing afterwards.
- **The Tauri example driver host keeps running while its window is hidden
  (finding 212).** macOS suspended the occluded WKWebView's page while its
  socket still answered pings, so a backgrounded host looked connected but ran
  no command. The example window now sets `backgroundThrottling: "disabled"`.
- **Fixed `h10-capture` failing with `scan.already-active` on a fresh host
  (finding 209).** The capture left its advertisement scan open (deferred to
  teardown), and `find()` then opened a second scan, which one manager refuses.
  The capture now stops its scan, and waits for the release, before `find()`
  on every loop exit. `scan.already-active` refusals now name the occupying
  scan (session, owner lease, state) on the desktop and React Native providers
  and in the desktop central, so a scan left open is diagnosable.
- **The Electron supervisor reconnects after a drop-link (finding 211).** Main
  removes subscriptions automatically when their source ends terminal. The
  supervisor's explicit release after the loss was therefore denied
  (`ownership.denied/electron-main-router.subscription-ownership`), and the
  supervisor parked in `cleanup-failed`. Releasing an already-removed handle
  again now reports `released`; handles that main never issued are still
  denied. Cleanup succeeds, and the supervisor backs off and reconnects like
  every other host.
- **Desktop scans keep reporting known peripherals (finding 205).** A
  peripheral that had been connected stopped appearing in name-filtered scans
  on macOS: CoreBluetooth replaced the advertised name with the GAP name read
  over the connection. The merged scan name and nameless sightings now track
  the advertised name, and every desktop scan re-observes each known
  peripheral once at scan start (previously BlueZ only). The Tauri 2 s cadence
  is unchanged.

- **Web discovery failed on descriptor-less characteristics and the chooser
  peer name was lost (finding 188).** Web Bluetooth's `getDescriptors()`
  rejects with `NotFoundError` when a characteristic has no descriptors
  (Polar H10 on Chrome): that is now an empty descriptor list, while any
  other descriptor error still fails discovery with its own code. The
  chooser-selected peer now carries the browser's `BluetoothDevice.name`
  (`null` only when the browser withholds it — Web Bluetooth exposes no
  advertisement payload for chooser devices). Pinned by
  `__tests__/web/web-bluetooth-descriptor-absence.test.js` with a fake
  `navigator.bluetooth`.

- **Chooser readiness timed out on Web (finding 187).** The system chooser
  is itself the permission step and the browser reports no radio power, so
  `adapter.waitUntilReady({ operation: 'choose' })` waited for a power-on
  that never comes. For `choose`, readiness is now availability plus a
  supported chooser (an explicitly unsupported chooser fails closed at
  once); every other operation still needs power on. The
  `deferredToChooser` workaround is removed from
  `examples-shared/driver/host.ts`, which waits like every other
  operation.

- **The tvOS stage kept a stale library copy and a stale Metro port
  (finding 176).** `example-expo/scripts/build-tv.sh stage` now drops the
  staged `node_modules/unified-ble-manager` so the next install resolves
  the `file:` dependency fresh from the repo instead of failing closed at
  runtime with `protocol.incompatible native-identity`; a new
  `verify-identity` step (also in `all`) fails loudly when the staged
  build identity differs from the repo's. `bundle-url` now replaces a
  stale override so `TV_METRO_PORT` wins in every step. Pinned by
  `__tests__/TvBuildStage.test.js` against a redirected stage.

- **A submitted CCCD write answered with Android status 129 is the stack's
  transient glitch, retried by the caller (finding 183).** It stays
  `platform.failure` (the link survives; a retry passes) but now reports
  `caller-decides` like finding 149's 133/HCI 0x3E, instead of `never`.
  Investigation ruled out our side: generation fencing stands, the old
  `BluetoothGatt` is closed at teardown, and the 129 arrived via callback
  on a live Gatt whose discovery and local registration succeeded. Pinned
  by a `ubm-mobile` failure-mapping test and Kotlin classification tests.

- **A failed scan stop no longer bricks the process with
  `scan.already-active` (finding 185).** The retained membership is now
  retried by the next start (surfacing the release debt when it still
  fails), and manager destroy always disposes the backend session — merged
  into the destroy record, never swallowed — so no scan lease survives a
  destroyed manager. Pinned by provider sequence tests
  (`rust-core-scan-lease.test.js`) and driver scenario sequences with
  fake managers.

- **A requested disconnect ends Tauri subscriptions `owner-released`
  (finding 190a).** The dispatcher aborted the notification tasks with no
  terminal, so the supervisor read `stream.closed` and stopped; the stream
  now ends with the vocabulary's requested-disconnect word, as on RN
  iOS/Android/tvOS, so the supervisor backs off and reconnects. `tauri`
  joins the event-vocabulary backends, extending the cross-host supervisor
  matrix. Pinned by a dispatcher test and `__tests__/event-vocabulary.test.js`.

- **Tauri advertises max-write/long-write like the desktop core (finding
  190b).** `gatt:maximum-write-length` and `gatt:long-write` are `limited`
  (measured per link through the shared core; prepared writes still rejected,
  never silently single-written), so `connection.maximumWriteLength` measures
  through IPC; the effective MTU stays `capability.unsupported` but now
  carries the desktop core's own reason
  (`effective-mtu-boundary-unavailable`) through the snapshot and the IPC
  projection instead of a bare refusal. Pinned by a snapshot test,
  `__tests__/TauriManager.test.js`, and
  `__tests__/ipc/capability-bootstrap.test.js`.

- **Every Tauri discovery of a database with descriptors failed with
  `protocol.violation` at `public-gatt.duplicate-characteristic-path`, and
  GATT topology rejections now name the offending path (finding 182).** The
  Tauri dispatcher rendered one IPC characteristic record per core discovery
  row, and descriptor-level rows repeat their characteristic's identity — so
  every characteristic with a descriptor (on the Polar H10: heart-rate
  measurement and battery level CCCDs, device-info user descriptions, PMD ECG
  CCCDs) reached the public snapshot as duplicated paths. The dispatcher now
  renders one record per characteristic — the same grouping the desktop N-API
  path applies — with identical occurrence numerals, and descriptors reference
  the single characteristic handle. A descriptor row without its
  characteristic row fails closed as `tauri.discover-descriptor-parent`
  instead of emitting an empty record. Rediscovery still replaces the
  snapshot: the previous database goes stale (`gatt.stale-handle`) rather
  than appending. Pinned by `finding_182_*` dispatcher tests over an
  H10-shaped scripted database and by `__tests__/tauri-gatt-database.test.js`
  (Tauri/Electron IPC codec to public snapshot, plus a
  connect-discover-discover stack test). `duplicate-characteristic-path`,
  `duplicate-service-path`, `characteristic-parent`,
  `duplicate-descriptor-path` and `descriptor-parent` carry the uuids and
  occurrences in the error's `platform` detail (`domain: 'gatt'`) and safe
  message, never only a bare code.

- **Every filtered Tauri scan failed with `protocol.malformed` at
  `tauri.scan-query` ("normalized scan query digest is invalid"); only
  unfiltered scans worked.** The Rust decoder recomputed the query digest
  over a canonical JSON that drops null fields, while the TypeScript
  normalizer (the contract owner) keeps null `services`/`names`/
  `manufacturerData`/`serviceData`/`rssi` and drops only null
  `peers`/`addresses`. The plugin now reproduces the TypeScript canonical
  form byte-identically for every query shape — services any/all, names
  exact/prefixes, manufacturer and service-data patterns, radio addresses,
  RSSI bounds, connectable, peers, exclusions, multi-clause ordering,
  unicode names, and UUID case/short forms — and accepts the explicit wire
  nulls the TypeScript encoder sends for one-sided RSSI bounds and
  prefix-less byte patterns. Radio `addresses` also crossed the IPC
  boundary for the first time: the TypeScript scan-query encoder omitted
  them (breaking address-targeted queries on Tauri and Electron), and the
  Rust decoder did not know the field. Pinned by the shared golden corpus
  `__tests__/fixtures/scan-query-digests.json` (generated by
  `scripts/generate-scan-query-digest-corpus.js`), checked by a Rust test
  that decodes the exact webview wire bytes and by a TypeScript test that
  pins the fixture to the live normalizer. No test caught this before
  because the only Rust digest test computed its digest with the same
  divergent code and covered a services-only clause.

- **Example driver link-loss outage completes whichever event carries the
  reconnect generation first (finding 162).** The outage matched the
  reconnect only on the supervisor `connected` event, so when the first
  value with the new generation arrived first (observed on Android), the
  record closed with `reconnectMs` / `firstValueAfterReconnectMs`
  permanently null. The scenario now records the reconnect on the
  supervisor `connected` event, the lifecycle `connected` transition, or
  the first value with the new generation — whichever arrives first (a
  value-observed reconnect fills `reconnectMs` from the value time with
  `firstValueAfterReconnectMs` 0) — and a late duplicate loss for a
  superseded generation no longer opens a ghost outage. Covered by
  `__tests__/driver/link-loss-outage.test.js`.

- **Committed Android prebuilts rebuilt from the current Rust sources
  (finding 157).** `android/src/main/jniLibs` shipped echo/gatt-only
  binaries with none of the `MobileCoreBridge` entrypoints the Kotlin side
  calls, so opening a session from the prebuilt failed with
  `UnsatisfiedLinkError`. Both ABIs (`arm64-v8a`, `x86_64`) are rebuilt via
  `android/refresh-prebuilt-jniLibs.sh` with a sealed `build-identity.json`,
  and `__tests__/AndroidPrebuilds.test.js` now seals every Kotlin-declared
  JNI symbol per `.so` plus `native-build-identity --check-android-prebuilts`.

- **A throwing drain sink ends the router instead of escaping as an
  unhandled rejection.** A `deliver` throw is a fatal delivery defect that
  ends the router, but it escaped from the `wake()` path (which owns no
  await) as observable noise on React Native. The router now stops at the
  second throw, reports the defect once through `sink.failed`, and `stop()`
  resolves.

- **A link lost during service discovery is `connection.lost` on
  Android, and an unrequested disconnect is a loss.** Android's driver
  answered discovery with a Boolean, so the link loss that failed a pending
  discovery reached the app as `platform.failure: gatt.discover` (legacy
  reported `connection.lost`). `OwnedAndroidGattRadio.discover` now answers
  a `Result` and the failure crosses unchanged (`AndroidGattLinkLost` →
  `not-connected` → `connection.lost`). The mobile owner also keeps the
  status of a disconnect: a disconnect Android reports with a non-zero GATT
  status, or CoreBluetooth with an `NSError`, is a link loss (`link` reason
  `peer`, lifecycle `lost`) even while a release is pending (new
  `RadioEvent::Lost`); before, it read as the app's own release. Native
  rebuilds: the Android app (Kotlin) and both mobile `RustCore` builds.

- **React Native managers report `discovery:continuous-scan`.** The Rust
  route (and legacy React Native before it) never registered it, so an
  Expo manager said `discovery.kind: 'system-chooser'` and an application
  that read the capability called `choose()` (`capability.unsupported`)
  while `find()` scanned fine. Android and iOS now register it `limited`;
  the system chooser stays unsupported. Web Bluetooth now also answers the
  shared ids — `discovery:system-chooser` `limited`,
  `discovery:continuous-scan` `unsupported` — beside its `web:*` ids.

- **Example driver:** Fast Refresh / HMR disposes the driver
  (`ScenarioRegistry.stopAll()`), so an orphaned run never keeps a
  connection; single-shot scenarios retry a `caller-decides` connect once,
  reported as a `connect-retry` event; every peer-acquiring scenario takes a
  `device` argument, and sequences bind a strap per target (`devices`,
  `parallel-two-straps.json`). See `examples-shared/driver/README.md`.

- **React Native Rust core: `find` no longer rejects `stream.overflow` when
  several advertisements arrive together, and notifications keep flowing with
  the screen off.** The drain takes up to 256 records per native call and
  delivered them in one synchronous loop, so three advertisements in one
  batch overflowed a `latest` stream (one item) before its reader re-armed —
  `manager.find({ query })`, which reads every advertisement through that one
  slot, rejected ~400 ms into a scan on a Samsung Android phone. (Motivation,
  not a support claim.) Data records (`adv`, `value`) now reach JS one per
  native→JS task, as 4.x delivered one native callback per record: taken
  records wait in a backlog, and the boundary before each further data record
  is a one-record drain call, whose promise resolves as a task of its own
  while the app is in the background. No JS timer is involved (an interim
  `setTimeout(0)` boundary held values ~26 s after screen lock, because React
  Native timers stop with the host). Control records keep their drained order
  and neighbours, so a link loss still ends its subscriptions
  `connection-lost`. Destroy drains every record the owner still holds before
  it resolves. The desktop provider and the Electron/Tauri IPC clients already
  deliver one record per native completion or IPC message.

- A disconnect that is retried successfully no longer leaves a stale
  `ReleaseFailed` in the destroy record (finding 38). Unsubscribe after a
  service change still disables the OS CCCD (finding 40). A dispatched write
  that fails with any error code reports commit `unknown` and is never
  retryable (finding 41).

### Added

- Deterministic reliability acceptance: shared-scan cancel/join, same-peer
  intent race, disconnect-during-subscribe fencing, generation-fenced
  reconnect, bounded slow-drain, and a 200-cycle ownership soak across
  deterministic, RN android/apple, and desktop synthetic legs; a React Native
  throughput benchmark (1/3/6 streams). Deterministic-only evidence, no radio
  proof.
- The example Expo app opts into restoration so the `restoration` scenario
  can be tested physically: the plugin option for iOS `restoreIdentifierKey`
  and Android companion presence in `example-expo/app.json`, with the
  re-prebuild build step documented in `example-expo/README.md`. Pinned by
  `__tests__/ExampleExpoRestorationConfig.test.js`.

- Desktop Rust path: `connection-lost` / `database-changed` events, adapter
  power and authorization with a watch, adapter enumeration and selection,
  connected RSSI (macOS), and maximum write length plus long write.
  Windows/Linux gain security state, pair, cancel pairing, unpair and
  security events, plus the BlueZ pairing-generation controller. Linux gains
  address targeting (`connections.peerFromAddress`), advertisement address
  types, and extended characteristic flags and access requirements. macOS
  gains advertisement solicited/overflow UUIDs and `connectable`. Windows
  gains maintain-connection. Each capability is registered only where the
  loaded core implements it on this OS (`UbmCentral.capabilityStates`).
- `scripts/ci/napi-clean-tarball-acceptance.js`: clean packed-consumer
  acceptance for the desktop core (npm/pnpm, CJS and ESM, a foreign cwd, no
  Rust on PATH), with `--negative` refusal legs and a `--probe radio`
  hardware leg.

- `BleError.retryability` (`'never' | 'caller-decides'`) exposes the
  operation's own answer about repeating it, rehydrated from the normalized
  error across every host boundary; errors built without an answer default to
  the code's retryability, so existing construction is unchanged. New exports:
  the `BleRetryability` type (root and `/backend-sdk`), `recoveryForError` and
  `BleRecoveryInput` (`/backend-sdk`), and the `{ kind: 'verify-state' }`
  `RecoveryAction`.
- `BleError.commit` (`'not-dispatched' | 'uncertain' | null`) exposes the
  commit state the operation's owner reported, rehydrated from the normalized
  error; it is `null` when the owner did not know or did not say. New exports:
  the `BleCommitUncertainty` type (root and `/backend-sdk`), the optional
  `commit` on `BleRecoveryInput`, and the optional `commit` on the public
  cleanup `NormalizedBleError`.
- Tauri wire additions:
  - every normalized error from the plugin (route failures, stream terminal
    errors, cleanup failures) carries `commit`: `not-dispatched` (nothing
    reached the radio), `uncertain` (a dispatched write may have reached the
    peer), or `null`. The transport and IPC manager validate the vocabulary
    and reject any other value as `protocol.malformed`; `NormalizedBleError`
    gains the optional `commit` field, which `serializeNormalizedError`
    carries only when it is set;
  - subscriptions report `delivery` (`notification`, `indication`, or
    `unknown` when the platform does not say) in the subscribe response and
    on each value;
  - the plugin forwards the core's connection-lifecycle events to the
    connection-event stream: OS link loss (`connected → lost`, cause
    `peer-link-loss`, stream ends `connection-lost`), requested disconnect
    (`disconnecting → disconnected`, cause `requested-disconnect`, stream ends
    `owner-released`), and GATT service change (the connection's database
    becomes stale); falling more than 256 events behind ends every
    connection-event stream with `overflow`;
  - `budgetMs` must be a non-negative safe integer or absent; any other value
    fails with `protocol.malformed` before any effect. Without a budget the
    plugin applies liveness backstops (120 s connect, discovery and GATT; 10 s
    scan stop, unsubscribe and disconnect; 30 s OS scan start), reported as
    `operation.timed-out` with the detail `liveness-backstop`.
- 5.0 known-peer restoration (issue #212): restoration reconnects directly to
  devices the app already connected to, with the same public events and
  vocabulary on iOS and Android and no auto-reconnect. iOS replays
  terminated-app state through `willRestoreState` (`peers.restored`,
  `restoration-received`, `restoration.claim()`); Android arms per-peer
  Companion Device Manager presence (`presence.observe` /
  `presence.unobserve`, API 31+, `connectGatt(autoConnect = true)` intent
  `'when-available'`) and the Expo plugin declares the permission-gated
  `UbmCompanionPresenceService`. New `state:restoration-adoption` /
  `state:presence-observation` capabilities (unsupported platforms report
  `capability.unsupported` with a reason), `restoration-received`
  event-vocabulary rows, the shared `restoration` example scenario, and the
  owner physical-device procedure in `docs/BACKGROUND.md`.
- Expo Apple TV (tvOS) prebuild support: with `EXPO_TV=1` (via
  `@react-native-tvos/config-tv`) the Expo config plugin omits
  `bluetooth-central` from `UIBackgroundModes` and writes no restoration
  id/generation keys — tvOS has no background Bluetooth mode and no state
  restoration — while phone prebuilds are unchanged. The runtime then reports
  both capabilities truthfully (`capability.unsupported` /
  `capability.unavailable` with the native reason). The Expo test-driver
  adapter reports Apple TV as platform `tvos` (react-native-tvos keeps
  `Platform.OS === 'ios'` with `Platform.isTV`), and the example app builds
  from the generated `example-expo/ios-tv` stage
  (`example-expo/scripts/build-tv.sh`, `docs/TVOS.md`). No library radio
  change: the podspec already declared tvOS and the full pod target compiles
  and archives for tvOS arm64.
- Android TV: the Google TV emulator (API 36) is a repeatable Expo test host
  via `bash example-expo/scripts/android-tv-emu.sh all`. It proves
  adapter/scan truthfulness only (clean 0-observation scans); it is not
  physical-radio evidence.

## [4.0.28] - 2026-09-09

### Fixes

- React Native Apple hosts now keep one process-owned CoreBluetooth restoration
  radio for each immutable native configuration. Reconstructing the TurboModule
  borrows that radio instead of registering another `CBCentralManager` with the
  same restoration identifier, avoiding duplicate-restoration ownership and
  preserving the OS restoration provider across JavaScript runtime replacement.
- Apple protocol attachments claim callback delivery exclusively and fail closed
  if another attachment is active. Closing or invalidating an attachment releases
  only that borrower's scans, subscriptions, pending operations, and non-restored
  connections; it no longer destroys the process-owned restoration provider.

## [4.0.27] - 2026-09-06

### Fixes

- Tauri continuous scans now pace synthetic peripheral-inventory snapshots at
  two-second intervals instead of replaying the full inventory four times per
  second. Real btleplug advertisement events remain event-driven. This keeps
  discovery live when platform events are silent or closed while sharply reducing
  bounded IPC pressure without suppressing real `duplicates: "all"` reports.
  Slow inventory work delays the next poll instead of triggering catch-up bursts.

## [4.0.26] - 2026-09-06

### Fixes

- A third-party backend can now follow, and pass, the cancellation contract
  4.0.7 introduced. `cancelOutcomeForPairResult` was exported from
  `backend-contract/security` but re-exported type-only from the barrel, so it
  never reached `/backend-sdk`; first-party backends imported it by deep
  relative path and nothing failed. The TCK still hard-required
  `outcome === 'cancelled'` from both `pair()` and `cancelPairing()`, so a
  backend that correctly reported `'paired'` when the bond won the race failed
  `security-pairing-cancellation-cleans-up` while conforming. The scenario now
  asserts that the two calls do not contradict each other about one pairing,
  checked through the shared mapper rather than against a fixed word.
  `cancelOutcomeForPairResult` also gains a `default` branch so an out-of-contract
  outcome from a third-party backend is named as `protocol.violation` instead of
  surfacing as `undefined` and a raw TypeError several frames away. The `default`
  routes through a `never` parameter, so a new first-party `SecurityPairResult`
  variant is still a compile error (#167).

## [4.0.25] - 2026-09-05

### Fixes

- Tauri: separate live correlation capacity from 30s replay tombstones. Routed
  cleanup still admits after a full replay window. Live exhaustion is retryable
  backpressure, not `protocol.violation`. Replay of an old correlation still
  rejects.
- Tauri: a native connect that completes after its caller/lease is gone gets an
  orphan cleanup owner before compensating disconnect. Reservation and owner
  release together, and only for the matching generation.
- Tauri: scan-stream failure keeps the scan owner in a stopping state until
  native stop settles. A failed stop stays retryable and blocks a second scan.
  Event and poll emit failures carry structured native diagnostics on the
  shared `source-failed` terminal without inventing drop counters (#173).
- Tauri: post-await lease/generation checks on unsubscribe and scan stop.
  Old-generation resources quarantine instead of attaching to a replacement
  caller. Compensating unsubscribe after subscribe success uses the same orphan
  owner instead of discarding the outcome.
- Tauri: overlapping stop requests for the same scan join one native
  `stop_scan()`. Completions carry an attempt token, so a late stop of scan A
  cannot take scan B’s stopping owner.
- Core: retain unadopted scan/connect leases when stale-admission `stop`/`release`
  fails, and retry that exact lease on later manager cleanup. Aborted, destroyed,
  or deadline-expired callers settle promptly; native work stays owned until
  adopted or compensated.
- Core: coalesce concurrent `rediscoverGatt` waiters onto one replacement after
  a shared in-flight discovery settles. A starter abort or timeout does not fail
  a sibling waiter with its own admission.
- Public scan observation overflow is subscriber-local. Consuming only `events`
  at full speed with `duplicates: 'all'` and `overflowPolicy: 'error'` no longer
  terminates the scan because an unused observation queue filled.
- Drop-policy public scan overflow (`balanced` / `latest` / drop-\*) reports an
  overflow notice and keeps scanning. The radio stays up. `overflowPolicy:
'error'` (`lossless-bounded`) still fail-closes the consumed view as
  `failed`/`overflow`; physical `stop()` remains cleanup.
- In-process and IPC scan sessions leave `active` when the host source ends
  without `stop()`. An already-terminal source never publishes `active`; the
  first state is the projected terminal (`failed` for `source-failed` /
  `connection-lost` / `overflow`, `stopped` for ordinary close). That event is
  ended delivery; `stop()` remains the physical cleanup path. The data pump
  still drains advertisements already accepted by `finishWithReason` before
  the observation terminal. Structured `source-failed` errors survive scan-state
  binding and reach `scan.observations`.
- BlueZ `StopDiscovery` / `StopNotify` release this client’s session even if
  another D-Bus client keeps `Discovering` / `Notifying` true.
- BlueZ `StartNotify` confirmation failure after native accept keeps a retryable
  `StopNotify` owner instead of orphaning the notify session. A late enablement
  success does not promote `removing` / `enabling-failed` back to `ready`.
- BlueZ `SetDiscoveryFilter`, `StartDiscovery`, and `ConnectDevice` honor caller
  deadline/abort without dropping a later native allocation.
- BlueZ keeps a retryable compensating `Disconnect` when `ConnectDevice`
  succeeds after the caller is gone. A failed first Disconnect is retried on
  destroy instead of being logged and forgotten.
- BlueZ address-connect fallback checks scan-plan compatibility, waits out a
  stopping scan, and widens an excluding live scan for concurrent address
  waiters, restoring the owner filter only after the last borrower leaves
  instead of waiting under a filter that cannot observe the target.
- Apple CoreBluetooth rejects an independent GATT read while that characteristic
  is notifying, because `didUpdateValueFor` cannot distinguish a read response
  from a notification. Electron/Node CoreBluetooth uses the same policy. A read
  admitted while idle still completes from its ATT response if CCCD enable
  races it; overlapping reads and subscribe-while-read are rejected; fused
  values that cannot be attributed are dropped. Remaining native reject codes
  (iOS 1031/1011/1032, Electron 413/414/415) map to `gatt.read-failed` or
  `gatt.subscribe-failed`, not `platform.failure`.
- Android `setCharacteristicNotification(...) == false` is a local registration
  failure, not proven link loss. Generation-matched disconnect and GATT status 19
  remain the physical-loss authority. The 4.0.22 250 ms CCCD/disconnect two-signal
  arbitration is unchanged. Notifications that arrive after the CCCD write and
  before `onDescriptorWrite` SUCCESS are staged and flushed to admitted
  subscribers. A later same-generation CCCD SUCCESS claims the arbiter instead
  of leaving the peer enabled after a failed public subscribe.
- WinRT stages `ValueChanged` notifications that arrive before CCCD enablement
  completes, then flushes them once and in order only to admitted subscribers.
  Staging continues until every pending waiter is admitted, and a per-subscriber
  overflow does not discard siblings’ pending buffer.
- WinRT subscription invalidation waits are bounded. If native `startNotify`
  never settles, disconnect still completes within the cleanup budget. A late
  enable issues a compensating `stopNotify` when the native CCCD key is vacant,
  and skips that disable only when a replacement generation already occupies it.
  Native notification map entries are keyed by connection generation.
- `cancelPairing()` honors abort and deadline on Android, WinRT, and BlueZ.
  Admission runs before native cancellation. A timed-out wait is not reported as
  `'cancelled'`. An abandoned waiter leaves the in-flight pairing owned.
  `pair()` abort/deadline keeps the pairing owned until the native result;
  a later native `paired`/`rejected` is authoritative for both `pair()` and
  `cancelPairing()`.
- Web adapter `watchState()` follows browser `availabilitychanged` instead of
  staying stale until another API samples availability. When that event is
  missing, watches share one bounded `getAvailability()` poll. A completed
  availability sample is applied unless a newer sample has already been applied,
  so an outstanding later probe cannot make `attach()` throw `adapter.unavailable`
  from a stale unavailable cache.

## [4.0.24] - 2026-09-05

### Fixes

- Arbitrate an Android characteristic-write callback failure against an
  already-in-flight disconnect callback. A provisional write status such as
  133 is retained for a bounded 250 ms evidence window, allowing the
  authoritative connection-state callback to report typed `connection.lost`
  without relabeling an isolated status 133 as link loss.
- Preserve the Android GATT operation and status for synchronous and
  asynchronous characteristic-write failures that are not proven link loss.
- Make exact and UUID-keyed pending-write cleanup identity-safe so stale
  teardown and late callbacks cannot settle a replacement write or remove its
  payload.

## [4.0.23] - 2026-09-05

### Fixes

- Share one adapter-state polling owner across IPC state watchers and readiness waits, sampling at
  a 500 ms control-plane cadence. The previous 25 ms polling loop could fill
  Tauri's bounded native correlation window and prevent scan cleanup or other
  operations. Watchers retain independent cancellation and bounded streams;
  stopping the last watcher releases polling, and stale reads cannot affect a
  replacement watch owner. Native replay protection remains unchanged.
- Honor readiness deadlines shorter than the polling interval and abort a
  pending readiness delay immediately without issuing an extra native route.
- Cancel event-driven readiness waits promptly in the shared public wrapper,
  preserving `operation.aborted` even when the underlying stream closes first.
- Bound and retain pending backend state-watch acquisitions. Aborted or timed-out
  readiness calls no longer wait indefinitely for Web availability probes; late
  resources are cleaned up and failed cleanup remains retryable during teardown.

## [4.0.22] - 2026-09-04

### Fixes

- Arbitrate an Android asynchronous CCCD failure against an already-in-flight
  disconnect callback for the same GATT generation. Android can deliver status
  133 from `onDescriptorWrite`, accept local notification rollback, and only
  then deliver the authoritative status-19 disconnect; exact subscriptions now
  retain that provisional failure for a bounded 250 ms evidence window so the
  command reports typed `connection.lost` without hiding genuine CCCD errors.

## [4.0.21] - 2026-09-04 (unpublished)

Publication was cancelled before the npm version check after physical Android
hardware reproduced a later callback ordering that required the 4.0.22 fix.

### Fixes

- Preserve Android GATT status 19 when a peer disconnects while an exact or
  non-exact CCCD subscription is awaiting `onDescriptorWrite`. The pending
  subscription now reports typed `connection.lost` instead of generic
  `platform.failure`; asynchronous CCCD status 133 remains an ordinary failure
  when local-registration rollback succeeds.
- Preserve that typed primary link-loss result when exact CCCD rollback also
  fails; the cleanup failure remains separately reported and retryable.
- Classify an exact synchronous CCCD descriptor-submission rejection as link
  loss only when the subsequent local-registration rollback is also rejected;
  genuine subscription rejections remain ordinary failures.
- Apply the same two-signal rule when Android accepts descriptor submission but
  its asynchronous CCCD callback fails before the connection-state callback:
  a rejected rollback proves the link is gone, while a successful rollback
  preserves the original platform failure and GATT status.

## [4.0.20] - 2026-09-03

### Fixes

- Treat Android's synchronous `setCharacteristicNotification(...) == false`
  result as typed `connection.lost` without consulting
  `BluetoothManager.getConnectionState`. Android can reject registration after
  the GATT link is gone while that manager query still reports connected.
  Exact and non-exact notification paths now share this behavior; asynchronous
  CCCD failures retain their native status and remain ordinary subscription
  failures unless Android reports link loss.

## [4.0.19] - 2026-09-03

### Fixes

- Normalize a synchronous Android notification-registration failure as
  `connection.lost` when the platform already reports the GATT peer as
  disconnected. This closes the callback-ordering window where a peer could
  disconnect before Android delivered `onConnectionStateChange`, while
  preserving ordinary registration failures as `platform.failure`.

## [4.0.18] - 2026-09-03

### Fixes

- Let exactly one lifecycle path complete a React Native subscription command
  when peer disconnection races a late CCCD callback. Android atomically
  claims the pending command before emitting a failure, while Apple ignores
  subscription completions from a disconnected connection generation.

## [4.0.17] - 2026-09-03

### Fixes

- Downgrade expected late native connect-cleanup diagnostics after caller
  cancellation from error severity to informational telemetry, while retaining
  the physical connection quarantine until native teardown is confirmed.

## [4.0.16] - 2026-09-03

### Fixes

- Normalize an Android CCCD operation that terminates with GATT status 19 as
  `connection.lost` even when it arrives before the native `connectionLost`
  event. Other CCCD failures remain `platform.failure` and retain their native
  GATT status.
- Cancel a dispatched React Native Android `when-available` connection when
  its caller aborts, releasing the native pending connection so a later
  same-peer retry is admitted without affecting distinct-peer connects.

## [4.0.15] - 2026-09-02

### Fixes

- Silence exactly one expected late React Native Android terminal when a peer
  is lost while an operation such as GATT discovery is still settling. The
  boundary records only operations it explicitly rejected for that peer,
  bounds and lifecycle-scopes those records, and continues to report unknown
  or duplicate terminals as protocol diagnostics.

## [4.0.14] - 2026-09-02

### Fixes

- Release Android GATT ownership immediately when Bluetooth powers off or
  resets instead of waiting for a `STATE_DISCONNECTED` callback that Android
  may never deliver. Failed native closes remain retained and retry when the
  adapter returns, protocol routes are invalidated before adapter-state
  publication, and an ordinary disconnect timeout now also publishes the
  missing terminal state before cleanup. A cleanup unsubscribe after adapter
  loss is idempotent when native ownership is already gone.
- Terminalize adapter-loss connections exactly once with the typed `adapter`
  reason when a native CoreBluetooth-style or WinRT disconnect completes
  without a platform loss callback. Delayed native callbacks are ignored, and
  WinRT connection paths retain the attachment that created them.
- Treat an operation that is still settling after adapter loss as expected
  quarantine rather than a cleanup error. Cleanup still waits and retries, but
  only an actual native release failure produces an error diagnostic.

## [4.0.13] - 2026-09-01

### Fixes

- Accept the versioned notification-delivery field on native `subscribe`
  commands while continuing to reject it on `unsubscribe`. This fixes Android
  notification setup failing before CCCD dispatch in 4.0.12.
- Advance the Native Protocol ABI to 7 so JavaScript built for delivery-mode
  commands rejects an older native binary during handshake instead of failing
  later during the first subscription.

## [4.0.12] - 2026-08-31

### Fixes

- Use React Native's architecture-neutral JavaScript call-invoker holder for
  the Android JSI protocol transport. Native result and notification delivery
  now work under both bridgeless and legacy React Native runtimes without
  changing the public API or wire protocol.
- Preserve the requested notification or indication delivery mode through the
  React Native Android backend and native dispatcher instead of allowing the
  native layer to silently choose a different CCCD mode.
- Correct React Native protocol ownership and teardown races: binary write
  payloads are released by exactly one owner, connection loss rejects pending
  operations with the normalized lifecycle error, and an in-flight disconnect
  remains pending until the authoritative native teardown terminal arrives.
- Treat expected, typed direct-GATT link loss as lifecycle state rather than an
  unconditional console error across shared CoreBluetooth-backed hosts. The
  lifecycle event and recovery contract are unchanged.

### Documentation

- Add `docs/README.md`, a complete documentation map labelling every document
  Current, Historical, or Generated, and add `llms.txt`, a machine-readable
  package overview for AI coding agents covering the contract facts and every
  public entrypoint. Both are drift-gated: `pnpm docs:check` now fails when a
  document is missing from the map, a listed document does not exist, or
  `llms.txt` stops covering a public entrypoint.
- Label the remaining lineage documents (pre-4.x changelogs, delivered 4.0
  roadmap, tvOS 3.x spec, v1 README/migration) with explicit historical-record
  banners, and mark `docs/audits/`, `docs/review/`, and `docs/superpowers/` as
  frozen record directories.
- Add scoped agent guidance (`docs/AGENTS.md`, `scripts/AGENTS.md`,
  `native/AGENTS.md`) stating the conventions specific to those trees.

## [4.0.11] - 2026-08-30

### Documentation

- Document adapter interruption recovery through `manager.adapter.watchState()`
  in the README and connection guide, and add the same flow to the Expo
  example. UBM invalidates affected generations and leaves reconnect policy
  with the host; the example displays live adapter power, availability, and
  authorization instead of polling or silently reconnecting.

## [4.0.10] - 2026-08-30

### Fixes

- Preserve the originating native platform details when a shared direct-GATT
  cleanup failure is normalized. React Native Android adapter-loss diagnostics
  now retain their Android domain and code instead of being mislabeled as
  CoreBluetooth; Apple retains its CoreBluetooth domain, and host-neutral
  cleanup records keep the same public operation and retry-ownership contract.
- Build the complete package during `prepare`, so an explicitly pinned Git
  release branch exposes the same public entrypoints as the packed artifact.

## [4.0.9] - 2026-08-29

### Documentation

- Correct the README release-truth note so it no longer describes an already
  published version as merely being prepared. The wording now stays truthful
  while a future release branch is ahead of npm and keeps the registry and
  GitHub release as the publication authorities (#179).

## [4.0.8] - 2026-08-29

### Fixes

- **Apple runtime shutdown no longer retains a closed JavaScript attachment through its invoker.** Closing the native protocol now moves the `CallInvoker` out of shared state before scheduling runtime-thread sink cleanup, breaking the state/invoker/callback ownership cycle. iOS, macOS, and tvOS keep the same public behavior while clean teardown no longer leaves JSI functions alive past their runtime.

- **Public capability checks no longer hide implemented operations whose evidence is limited.** `manager.capabilities.supports(id)` and `require(id)` now agree with the core manager: both `supported` and invocable `limited` descriptors are usable, while `unavailable`, `unsupported`, and missing capabilities still fail closed. Callers that need full qualification can inspect `manager.capabilities.get(id).state` and its retained limitations. This fixes Android callers incorrectly discarding the bonded `when-available` reconnect path merely because its physical-radio evidence is still labelled `limited` (#174, PR #170).

- **Expo Android connected-device monitoring now has an app-controlled notification and explicit lifecycle recovery.** While an active background lease is held, `manager.background.updateNotification({ title, body? })` updates the existing UBM notification in place without acquiring or starting another service. The configured channel, icon, connected-device service type, ongoing state, session-intent policy, and host-app tap are preserved. `restart: 'while-session-intent-exists'` now also manages boot and package-replaced recovery, but only starts the service when native UBM session intent exists; it never scans or reconnects. Once a recovered service is promoted, a package-scoped `FOREGROUND_READY` signal lets an app-owned headless runtime resume without racing Android's background-service restrictions; normal acquisition resolves its existing caller without launching a redundant headless runtime. Background and restart remain absent/`never` by default, and unsupported hosts reject the operation truthfully (#177, PR #170).

- **React Native no longer closes a healthy notification subscription during a legitimate burst of native events.** Android and Apple still fail closed on an undrained native-to-JavaScript queue, but both bridges now retain up to 512 records / 1 MiB before declaring overflow. That bounded budget accommodates common catch-up transfers such as roughly 288 five-minute readings delivered in one 24-hour history response while preserving a visible `stream.overflow` terminal if JavaScript genuinely cannot keep up. The change applies only to the two React Native native-event ingress queues; other backends keep their existing buffering and capability truth (#175).

- **React Native Android now exposes the system bonded peer directory.** `manager.peers.bonded()` reads the ABI-6 Android bond table, returns deterministic version-1 system-scoped references, and `manager.peers.resolve()` rechecks that a saved reference remains bonded before reconnecting. `manager.connect(peer, { intent: 'when-available' })` reaches Android's queued auto-connect path. Bonded metadata does not imply reachability; Android reports unknown reachability and preserves `permission.denied` instead of returning an empty list. Other backends keep their truthful unsupported boundaries, and Web origin-authorized devices remain distinct from bonded peers.

- **React Native Android now honours `PairOptions.transport: 'le'` instead of discarding it.** The public security layer validated and forwarded the selector, but the React Native Android backend dropped it before the native command and always called parameterless `createBond()`. Explicit LE pairing now crosses native-protocol ABI 5 and invokes Android's transport-selecting bond operation with `BluetoothDevice.TRANSPORT_LE`; `'auto'` remains the platform default. Unsupported or rejected explicit selection fails closed rather than silently changing the request. Because Android API 36 cannot publicly query bond state by transport, only an LE-only device's existing bond satisfies an explicit-LE `already-paired` result; dual-mode, classic, and unknown devices retry the directed operation instead of risking a false success from a BR/EDR-only bond. Reflection-wrapped permission failures retain `permissionDenied`. BlueZ and WinRT already pair through BLE-only device objects, while Apple and Web keep generic pairing unsupported; those platform semantics are unchanged.

- **Every GATT notification was dropped on both React Native backends, and characteristic and descriptor reads with them.** A notification's payload is carried as a binary reference, and the protocol requires that reference to name the operation the event belongs to — the codec compares the two for equality (`requireBinaryCorrelation`). Both bindings stamped the subscribe's correlation on the event while minting the payload under a correlation of their own (`"notification:<subscription>:<ordinal>"` on Android, `"apple-notification:..."` on Apple), a combination that can never validate. Every notification was therefore refused inside our own codec before reaching a caller, so a subscription delivered nothing at all while the radio received the peer perfectly well. The read paths decorated their correlation the same way (`"read:<epoch>:<nonce>"`, `"apple-read:<nonce>"`) and failed the same check on the result record. All four sites now mint the payload under the owning operation's nonce, as the write path always did.

  Sharing one correlation across a subscription's notifications is safe: `OwnedBinaryPayloadStore` keys retained payloads by a freshly generated owner token and releases by that token, treating the correlation as metadata it cross-checks rather than as a unique key.

  The deterministic layer could not catch this, which is why it shipped: the React Native test doubles minted their own notification correlations and omitted the operation correlation entirely, so they modelled a record the native codec refuses to deliver, and the suite stayed green against a shape no device could ever produce. `ReactNativeAndroidProtocolBoundary` now enforces the same rule the native codec enforces, the doubles emit the shape the bindings actually emit, and the codec's own tests pin both the notification event and the read result. Live-device evidence showed the platform receiving a peer's reply chunks while the application received none (#168).

## [4.0.7] - 2026-08-26

### Changed behaviour

- `cancelPairing()` reports what the cancellation **achieved**, not what it requested, and can no longer contradict the pairing it cancelled. Every backend answered `'cancelled'` unconditionally the moment the cancel was dispatched, so a cancellation that lost the race told the caller no bond exists while one did — and a caller who believes that never looks again. It now reads the in-flight pairing's own result rather than forming a second opinion, which is what makes the two calls incapable of disagreeing: there is one source of truth and the cancellation reads it. `SecurityCancelPairingResult` gains the words to say so — `'paired'` when the bond completed before the cancellation arrived, and `'rejected'` (carrying the peer's reason) when the peer refused, because claiming credit for stopping something that stopped itself is the same substitution with the arrow reversed. A pairing that _fails_ is not given an invented outcome: `cancelPairing()` rejects with the error the pairing rejected with. `'paired'` deliberately matches `SecurityPairResult`'s `'paired'` — a bond exists as a result of this operation — and is not `'already-paired'`, which in that type means the peer was bonded _before_ the call; one word, one meaning, in both types. Applies to BlueZ, Android, WinRT **and** the deterministic `/testing` backend, since a mock that answers differently from every real radio lets a consumer's suite pass against a contract no device honours. WinRT additionally short-circuited _upstream_ of the shared mapper — when its dispatcher reported the operation already terminal, which is exactly the lost race, it answered `'not-pairing'`, contradicting its own `pair()` and making that word mean both "there was nothing to stop" and "it was already over" (#159).

### Additions

- **`PairOptions.secureConnections` can now actually select the LE pairing generation on BlueZ — behind a host-supplied privileged operation.** Some peripherals accept only LE Legacy and terminate the link on a Secure Connections pairing request; 4.0.6 gave the contract a way to say so but no backend that could honour it. `org.bluez.Adapter1` (BlueZ 5.85) exposes no Secure Connections property and `Device1.Pair` takes no parameters — the setting lives behind the kernel management socket's Set Secure Connections command, which requires **`CAP_NET_ADMIN`**.

  This package never acquires that privilege: it opens no management socket, shells out to nothing, and does not assume it is root. A library that silently escalates hands an application capabilities its author did not choose and cannot audit. The **host** supplies the operation via `BluezBackendProviderOptions.pairingGeneration`, which makes the escalation visible in the application that opted into it. Omit it and `'require'`/`'disallow'` keep failing closed exactly as in 4.0.6 — the default posture is unchanged.

  Three properties of the kernel setting a host must accept, documented in `docs/BONDING.md` rather than discovered: it is **adapter-wide**, not per-pairing, so every pairing on that controller uses the selected generation while it is held; it **outlives the process**, because the kernel keeps it until something sets it back; and a **failed restore never changes the pairing's outcome** — a bond that was created is reported as created, with the restore failure reported separately, because leaving a controller in LE Legacy and telling a caller they are not bonded are both serious and are different facts. Concurrent _directed_ pairings on one adapter are serialised so they cannot corrupt each other's restore value; an undirected (`'prefer'`) pairing does not take that lock and uses whichever generation is held, which `docs/BONDING.md` states beside the option.

  `security:pairing-generation` joins the capability catalog and is reported by the instantiated backend at runtime: `unsupported` with the platform reason when no operation was supplied, `limited` when one was — so two BlueZ backends on one machine legitimately answer differently. Evidence is deterministic tests only; this is **not** physical-radio proof, and the label says so (#144).

### Fixes

- A Tauri connect that succeeds physically but is then denied admission no longer strands the peer. Both compensation branches — the caller vanished, and the caller's lease went stale — removed the peer reservation _before_ attempting the disconnect and then discarded its result with `.ok()`. If that disconnect failed or hung while the peripheral stayed connected, the result was a connected peer with no owner and no handle: nothing could reach it, nothing could retry it, and the caller was told only that admission was denied. Compensation now follows the rule the explicit disconnect already follows — a bounded wait, a fresh state reading, and a D-Bus error naming a vanished device object read as evidence of release rather than as a failed question — and surrenders the reservation only when the platform proves the link is down. Genuine indeterminacy keeps it so a retry can reclaim the peer, and the admission error carries what compensation could not undo, which is the difference between "try again" and "a peer is stranded" (#146).

### Known limitations

- `pair()`'s abort path still reports `'cancelled'` without knowing whether the daemon bonded anyway. Learning the truth means waiting for the radio, and waiting risks a hang that a wedged daemon would inflict on the caller — which the suite explicitly forbids. Resolving it needs a vocabulary that can express "cancellation requested, bond state not yet known"; `state()` and `watch()` remain authoritative meanwhile (#157).

## [4.0.6] - 2026-08-26

The first release cut against a live peripheral on Android as well as Linux. Most of it is defects that only real hardware surfaced, several of them boundaries that discarded evidence and so presented a specific fault as silence. Does not retag `v4.0.5`.

### Fixes

- A Tauri disconnect that fails no longer leaves the peer owned when the platform can prove it was released. btleplug's CoreBluetooth `disconnect()` never resolves once the peripheral has left its internal map, so the call is bounded and the outcome decided by a fresh state reading: gone means released, still connected means the retry keeps ownership. On BlueZ the state reading is classified before it is stringified, because a removed D-Bus device object is not a failed question but the answer to it - previously that case was treated as unknowable and the peer stayed owned until the process restarted. Genuine indeterminacy - a D-Bus timeout, the daemon gone, the adapter pulled - still retains ownership (#145, #154).

- BlueZ no longer reports a pairing it could not stop as `cancelled`. `Device1.CancelPairing` rejections were all swallowed, so a refusal from bluetoothd — `org.bluez.Error.Failed`, `NotAuthorized`, a D-Bus timeout, the daemon gone — left the in-flight `Pair` running while `cancelPairing()` returned `cancelled` and the abort/deadline path did the same. A caller told no bond exists while one is still being made cannot recover, because it never learns to look. Only the two rejections that prove nothing is left running are treated as success — `org.bluez.Error.DoesNotExist` and `org.freedesktop.DBus.Error.UnknownObject`, an answer to the question rather than a failure to answer it, the same pair the Tauri disconnect path classifies for the same reason. Anything else now surfaces as `platform.failure` carrying the D-Bus error name (#143).

- `scan()` works on React Native Android again, and so does every other control-plane operation there. The Android dispatcher stamped a hardcoded `1` on field 1 of every result and event it built while the native codec has required version 2 since the schema was frozen, so all of them — `scanStarted`, a connect completing, a connect failing, a cancellation, a bond state change, a lost link — were rejected at the JSI boundary as version-incompatible and dropped. A scan therefore never received its terminal: the radio discovered the peripheral 38 times in one four-minute capture and the application was handed nothing. The stamp now comes from the generated `NATIVE_PROTOCOL_VERSION`, so it cannot drift from the version the codec validates against (#140).
- A record the codec refuses is no longer only written to logcat: the Android binding counts quarantined records per attachment and emits a `recordQuarantined` diagnostic in place of the record it could not deliver, so an application learns that the boundary discarded something it was asked to deliver (#140).
- A version-incompatible record names its kind, the version it carried and the version expected, so a quarantined record identifies its emitter instead of leaving four record kinds to choose between (#140).
- `adapter.waitUntilReady()` no longer reports a radio that is still starting up as one the caller is not permitted to use. The React Native Android boundary answers `authorization: 'unavailable'` until the radio publishes its first authoritative state, and the readiness gate maps that value to `permission.denied` — so the one API whose purpose is to wait that window out failed on the very state it exists to wait for, on a phone with Bluetooth switched on and all four runtime permissions granted. The pending snapshot now reports `authorization: 'unknown'`, the value its `availability` and `power` siblings already carried for the same reason: the absence of a measurement is never a denial, so no readiness gate may block on it (#156).
- BlueZ states its connection-control truth instead of omitting the concept: `connection:priority` and `connection:parameters` are now registered as explicitly `unsupported`, with limitations naming the BlueZ D-Bus gap (no LE connection-parameter API on `org.bluez.Device1`/`Adapter1`, BlueZ 5.85), the privilege requirement of the alternatives (`CAP_NET_ADMIN` kernel mgmt socket and root-only debugfs — neither a live per-connection update), and the consequence that GATT traffic may run at a multi-hundred-millisecond peer-negotiated interval. `requestPriority()`, `parameters()`, and `parameterEvents()` fail closed with that reason attached (`BleError.limitations` and `platform.safeMessage`) instead of a bare `capability.unsupported` that taught nothing while a slow link looked like a peripheral fault (#149).
- BlueZ `security.pair()` no longer fires `Device1.Pair` when the operation is aborted or times out while the just-works agent is still registering; it re-checks cancellation after agent registration so a cancelled pairing never proceeds on the daemon (#143).
- BlueZ `security.pair()` reports `paired` (not `cancelled`) when an abort or deadline lands after `Device1.Pair` has already completed the bond, so a bond that was actually created is never reported as if it never happened (#143).
- Android `security.pair()` no longer reports a timed-out pairing as `cancelled` when native cancellation is unavailable and it cannot actually stop the in-flight bond; the deadline path now fails closed with `capability.unsupported`, matching the abort path, so a bond that may still complete is never misreported as cancelled (#143).

### Additions

- `FindOptions` accepts `duplicates` and `delivery`, so the `find()` convenience can be pointed at a peripheral that advertises in dense bursts instead of being abandoned for a hand-driven `scan()`. Both default to the values `find()` has always used (`'coalesced'` and `'latest'`), so nothing changes for existing callers (#148).
- BlueZ pairing dispatch: `org.bluez.Device1.Pair`, `org.bluez.Device1.CancelPairing`, and `org.bluez.Adapter1.RemoveDevice` are now allowed through the dbus-next boundary, and the backend registers a just-works (`NoInputNoOutput`) `org.bluez.Agent1` on its own bus (not the system default) so a client-initiated pairing can complete without an external agent (#141, #143).

### Changes

- The Node convenience factories (`createBluezBleManager` and siblings) now select the first adapter (ordered deterministically by id) when the caller names no `adapterId`, instead of failing on a multi-adapter host. A single-adapter machine needs no configuration and a multi-adapter host picks the same controller every run; pass `adapterId` to target a specific controller (e.g. a second USB dongle used for debugging). The low-level provider is unchanged: it still requires an explicit selection and never silently substitutes one (#143).
- `PairOptions.secureConnections` (`'require' | 'prefer' | 'disallow'`, default `'prefer'`): request an LE pairing generation. `'prefer'` defers to the platform. No current backend exposes per-pairing generation selection, so `'require'` and `'disallow'` fail closed with `capability.unsupported` on BlueZ, WinRT, Android **and the deterministic `/testing` backend** rather than being silently ignored; the contract is in place for a backend that can honour it. The deterministic backend previously ignored the field and reported a Secure Connections bond under the exact option that forbids one, so a consumer's suite could pass against a contract every real radio rejects — the one way test infrastructure can actively mislead. A test that asserted a successful pair under `'require'` or `'disallow'` now sees `capability.unsupported`, which is what production would have done (#144, #143).

### Documentation

- Every hardcoded timing and capacity constant in `src/` is now classified as a protocol invariant, a safety bound, or host policy, and each one that stays fixed carries a comment at its definition saying why. Undocumented constants were the defect: a fixed deadline that is never explained reads as a device fault when a slower host misses it (#148).
- `find()`'s 10 s fallback deadline and `adapter.waitUntilReady()`'s 10 s fallback deadline are named constants shared between the in-process and IPC managers, so the same logical operation cannot expire at two different times either side of the IPC boundary (#148).

## [4.0.5] - 2026-08-25

First release cut against a live peripheral. Every fix below came from driving a real BLE device from an Android phone and a Linux/BlueZ host, and several are defects no unit test had reason to catch. Does not retag `v4.0.4`.

### Fixes

- `readiness()` no longer reports a working radio as `unavailable` before the Android adapter publishes its authoritative state; a boundary that has not yet been told the radio's state is a pending condition, not an absent radio (#116, #128).
- The direct-GATT backend is shared by CoreBluetooth and React Native Android, so its diagnostics now name the platform that is actually running — `[unified-ble:android-gatt.*]` rather than `[CoreBluetoothBackend.*]` — and its operation ids are `direct-gatt.*` instead of `corebluetooth.*`. Scan-stop cleanup timeouts carry the platform identity instead of `platform: null` (#117, #128, #132).
- An address known out of band can now enter the system: `ScanClause.addresses` and `connect({ address })`, gated on the reported `peer:address-targeting` capability, with pending BlueZ semantics that complete whenever the peripheral next advertises, and Android `ScanFilter.setDeviceAddress`. Hosts that cannot express a radio address — CoreBluetooth, Web, and the IPC transport — report the capability unsupported and fail closed rather than advertising something they cannot honour (#118, #128, #131, #133).
- `ScanOptions.platform` is honoured instead of unconditionally rejected, so consumers can select Android scan mode; peripherals that advertise infrequently are no longer effectively undiscoverable behind the platform default duty cycle. Gated on `scan:platform-options` in the public, core and IPC paths; `match-lost` and pre-26 `legacy: false` fail closed rather than silently doing something else (#120, #130).
- BlueZ GATT works again: occurrences are decimal indices rather than D-Bus object paths, so `discover()` no longer fails with `protocol.violation: public-gatt.occurrence` on every device — a regression introduced when the public validator was tightened without migrating the backend. Two same-UUID services are now distinctly addressable, which neither the old leniency nor the strict check managed (#123, #129).
- The BlueZ boundary decodes the `y` and `a{qv}` D-Bus variants that BlueZ 5.85 actually sends, and routes `Adapter1.ConnectDevice` through the boundary instead of rejecting it as locally unsupported; without these the backend could not attach at all (#128).
- Public BLE resources are portable across copies, and closing an adapter watch's value stream tears the watch down instead of leaving a 25 ms poll timer and an abort listener alive (#122).
- `@babel/runtime` is declared as a production dependency. The emitted CommonJS imports its helpers, so an external linked checkout failed with `MODULE_NOT_FOUND` while repository-local tests passed against a transitive copy — a packaging gap only a real consumer could surface (#134, #135).
- A database stream is finalized after an invalidation retry succeeds, and per-subscription React cleanup and failed IPC CCCDs stay owned (#127).

### Compatibility

- Native protocol ABI 3 → 4. A JavaScript bundle carrying the new scan fields now fails attachment negotiation against an older native binary instead of failing at scan-start — or, worse, silently selecting the opposite scan mode.

### Release integrity

- Cut from the exact `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels. The live-hardware runs behind these fixes are development evidence, not qualification evidence.

## [4.0.4] - 2026-08-25

Post-4.0.3 audit: wire/scan/IPC ownership, Android 16 KB ELF alignment, Apple teardown, abortable Web chooser honesty, React remount-owned cleanup, and React Native entropy without WebCrypto. Does not retag `v4.0.3`.

### Fixes

- Tauri wire codec budgets, public scan presence overflow, IPC contract errors, GATT admission/rediscovery, Android scanCallback/receiver commit-after-success, 16 KB native page size (#80, #81, #83, #84, #93, #85, #86, #100, #107).
- Apple disconnect confirmation, Service Changed CCCD retry, restored notify-off; Web chooser abort/timeout does not retain a late `requestDevice` grant (#87, #89, #90, #88).
- React adapter-watch fail-visible terminals, remount-owned scan/characteristic `release-failed`, scoped BleProvider barriers, composeAbortSignal listener cleanup, `managerKey` replacement (#96, #97, #98, #99, #101).
- React Native manager construction no longer depends on WebCrypto or a Metro-fatal `crypto` require; native CSPRNG is the default, with injectable `randomBytes` (#111, #113, #114).

### Release integrity

- Cut from the exact post-PR #110 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.3] - 2026-08-25

Lifecycle ownership is explicit through clone/decode, public stream close, IPC admission, Web/Tauri teardown, backend unregister/overflow native release, and React hook store/terminal cleanup. Does not retag `v4.0.2`.

### Fixes

- Serializable clone/decode stays fail-closed; public stream close, scan stop, and IPC/Web/Tauri teardown no longer drop `release-failed` or skip native disconnect (#58, #59, #75, #60, #72, #63, #73, #76, #74, #79, #67, #77, #78).
- Adapter, security, and event-stream close/overflow keep backend ownership until native release succeeds; overflowing CoreBluetooth scan owners do not stop remaining joiners (#61, #68, #69, #70, #71).
- React adapter-state watches cannot wedge or double-own a run; `useDiscoveredPeers` is bounded and honors lost-peer events; connection and characteristic hooks leave loading with a fail-visible terminal (#62, #65, #66).

### Release integrity

- Cut from the exact post-PR #105 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.2] - 2026-08-24

Public coalesced scans no longer lose duplicate suppression after lost-peer or presence-cap churn: fingerprint deletions now decrement the retained-byte counter. IPC pre-registration buffering is globally bounded (ID/item/byte/age) with fail-visible tombstones. Connection release attempts `connection.disconnect` even when lifecycle unsubscribe rejects or returns `release-failed`, and preserves both cleanup failures. Does not retag `v4.0.1`.

### Fixes

- Coalesced public scans keep exact fingerprint byte accounting across `reportLost` and presence eviction (#53).
- IPC pending streams for unknown IDs are aggregate-bounded; quota/TTL loss is visible when the ID later registers (#54).
- IPC connection release no longer skips physical `connection.disconnect` after a lifecycle unsubscribe failure (#56).

### Release integrity

- Cut from the exact post-PR #57 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.1] - 2026-08-24

Tauri JS transport now accepts the adapter-state snapshot the Rust plugin emits (`heard: null` on unsampled bootstrap, non-negative integer on live `adapter.state`). Extra or missing adapter-state keys stay fail-closed and are named in the decode error. Teaching docs describe the current stable 4.x install instead of RC-as-current. Does not retag `v4.0.0`.

### Fixes

- `createTauriBleManager()` no longer throws `protocol.malformed: tauri.transport.response` because Rust included `heard` on `adapter.state` (#50).

### Release integrity

- Cut from the exact post-PR #52 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0] - 2026-08-24

Stable package/API contract after PR11/RC5. Package SemVer `4.0.0` does not promote backend support labels or claim physical-radio evidence. Live-radio vertical slices, soak, and crates.io crate publication remain unverifiable in this environment and are recorded as blocked rather than mocked. Immutable `v4.0.0-rc.5` is not retagged.

### Qualification

- Deterministic, package, plugin, lint, and generated-artifact gates pass on the exact `main` candidate.
- Generated platform-support evidence stays Experimental / not bound to this artifact; compile/ABI/deterministic proof is not relabeled as live-radio.
- Tauri documented install remains crates.io (`tauri-plugin-unified-ble-manager@4.0.0`) with checkout `path` fallback until the crate is published.

### Release integrity

- Cut from the exact post-PR12 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as the stable `latest` package; this does not promote backend support labels.

## [4.0.0-rc.5] - 2026-08-24

Distribution and consumer-tooling freeze after PR11. Documented Tauri install is crates.io (`tauri-plugin-unified-ble-manager@4.0.0`); the crate is not yet published and `path` remains the checkout fallback until it is. Public CLI taxonomy is `doctor`/`inspect`/`init`/`support-bundle` plus `backend tck`/`backend scenario` routing. `TAURI_PLUGIN_COMPATIBILITY` and `createTestBleEnvironment` are exported from `/tauri` and `/testing`. This does not mint live-radio evidence or change RC2 portable BLE semantics or the RC4 Expo schema. Immutable `v4.0.0-rc.4.1` is not retagged.

### Packaging and CLI

- `ubm doctor` without `--backend` reports package/runtime identity and labels `proofBoundary: compile-config-loadability`.
- `ubm init --host tauri` writes a crates.io Cargo fragment; other hosts write stable public-API factory fragments and refuse overwrite without `--force`.
- `ubm inspect config|capabilities --host` and `ubm support-bundle create` do not load a radio or upload data.

### Release integrity

- Cut from the exact post-PR #48 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.4.1] - 2026-08-23

Fail-closed and IPC/governance closure from the RC3 audit on the post-PR10 `4.0.0-rc.4` line. PR11 / `4.0.0-rc.5` remains paused. Backend support labels remain evidence-derived and are not promoted by this release. Immutable `v4.0.0-rc.4` is not retagged.

### Public API and semantics

- Signature-aware API report checking; GATT included-service and property schema validation; connection-control echo validation; supervisor `stop()` no longer returns `released` while late configure owns the session; `BleCleanupError` retains the exact cleanup record; IPC local stream overflow tears down the remote producer; invalid Electron `deliveryMode` is rejected; Node host factories rehydrate to `BleError`.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics.

### Release integrity

- Cut from the exact post-PR #46 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.
- Repository: `main` is protected by ruleset `protect-main`; stale `agent/*` remotes were archived/deleted.

## [4.0.0-rc.4] - 2026-08-23

Expo host and native/residual scan-planning release candidate after PR10. This candidate includes the Expo config-plugin v2 schema, thin Expo factory and React lifecycle integration, optional Android foreground-service and companion association surfaces, iOS restoration identity, and the PR9 native/residual scan planner. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and host integration

- Completed the additive PR9–PR10 host surfaces while preserving the RC2 manager, GATT, error, capability, lifecycle, and scan-query contracts.
- Froze the Expo config-plugin and restoration configuration schema at this candidate. `createExpoBleManager()` remains a thin composition over the React Native factory/native provider.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics across native and host boundaries.

### Release integrity

- Cut from the exact post-PR10 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.3] - 2026-08-22

Advanced central release candidate after PR8. This candidate includes known-peer directories, connection intents and reconnect supervision, pairing/security semantics, advanced link controls, write readiness, and bounded GATT recovery. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and semantics

- Completed the additive PR5–PR8 central features while preserving the RC2 manager, GATT, error, capability, lifecycle, and scan-query contracts.
- Added explicit link-control observations and requests, connection-intent supervision, pairing/security result vocabulary, and safe service-change/cache-recovery behavior.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics across native and host boundaries.

### Release integrity

- Cut from the exact RC3 release commit `a423a73` on `main` through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.2] - 2026-08-21

Catch-up release candidate for the frozen 4.0 portable runtime contract. This candidate is cut from the exact post-PR5 `main` commit because the planned RC2 publication was delayed; it includes the PR1–PR5 contract and peer-directory work. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and semantics

- Completed the application-only manager, capability/recovery, GATT object, canonical scan-query, and scoped peer-reference surfaces.
- Added truthful backend peer-directory capability wiring, origin-authorized Web Bluetooth resolution, and fail-closed unsupported behavior for hosts without a proven directory boundary.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics.

### Release integrity

- Intended for publication as a prerelease candidate on the `latest` channel through the tag-driven trusted-publishing workflow.
- This is a release-train catch-up point; RC3 remains the post-PR8 checkpoint for the advanced central feature set.

## [4.0.0-rc.1] - 2026-08-19

Documentation correctness and pre-stable API fixes on the 4.0 contract. This does not promote backend support labels or claim live-radio evidence.

### Public API

- Removed invalid SIG read helpers `readHeartRateMeasurement`, `readBloodPressureMeasurement`, and `readTemperatureMeasurement`.
- Profile commands now reject missing characteristic properties with `gatt.property-not-supported` before calling the backend.
- Renamed Expo plugin option `isBackgroundEnabled` to `requiresBluetoothLeHardware` and rejected iOS `peripheral` background mode.
- Added `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG` (legacy `BLEPLX_PLUGIN_DEBUG` still enables plugin debug).
- Added application factories `createReactNativeBleManager({ clientId, managerId, hostSessionScope })`, `createNavigatorWebBleManager` default environment, and `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`.
- Added `BleManager.adapterStates()`, `defaultScanDelivery()`, `scanForServices()`, `withDiscoveredConnection()`, and `throwIfCleanupFailed()`.
- Renamed the injectable RN factory to `createReactNativeBleManagerWithEnvironment`.
- Default Web `visibilitychange` handling now reports `page-hidden` only when the document is hidden.
- `adapterStates({ signal })` re-checks abort after `watchState()` and closes the watch instead of leaking it.

### Documentation and examples

- Finite helper-first README Heart Rate journey, Expo/bare setup paths, and migration fixes for cancellation, coexistence, scan merge policy, and shared deadlines.
- Example BLE service lifecycle, overflow, path resolution, and stable client identity.

## [4.0.0-rc.0] - 2026-08-17

First publication from `sfourdrinier/unified-ble-manager`. This is the 4.0 package/API contract as a release candidate on npm `latest`, so `pnpm add unified-ble-manager` installs this build. It does not promote backend support labels or represent the stable 4.0.0 release.

### Stable package and public contract

- Established the Unified BLE Manager 4.0 package/API contract as its first release-candidate baseline.
- Established `sfourdrinier/unified-ble-manager` and `main` as the canonical repository and release branch.
- Preserved the Git ancestry of the 4.0 work while leaving `sfourdrinier/react-native-ble-plx` as the historical and 3.x home.
- Kept platform support qualification independent from package SemVer: stable `4.0.0` does not promote a backend beyond the support label justified by retained evidence.

### Package and host model

- Finalized the host-neutral root plus explicit React Native, Web, Electron, Node/CoreBluetooth, Node/WinRT, Node/BlueZ, backend SDK, testing, codecs, CLI, and profile exports.
- Standardized public BLE data on `Uint8Array`, cancellation on `AbortSignal`, explicit manager ownership, bounded event semantics, typed capabilities, and versioned backend/native protocols.
- Kept React Native, browser, Electron, Node, and third-party backend integration explicit; no production path silently falls back to Noble, Web Bluetooth, or a simulated backend.

### Release integrity

- Migrated canonical CI and release automation from the legacy `master`/`4.0` topology to `main`.
- Initial stable publication requires the release tag to identify the exact current `main` commit and reruns package, native-build, ABI, artifact, packed-consumer, and supply-chain checks before npm publication; post-publish recovery reuses the immutable npm tarball.
- Canonicalized package repository, issue, homepage, podspec, SBOM, and release metadata to the new repository.
- Canonicalized project licensing to Apache-2.0 and regenerated the SBOM and third-party license inventory from the final release metadata.
- Retained evidence-based platform labels without inventing physical-radio proof that has not been captured.

### Migration

- Reworked the README, migration guide, release guide, support/security guidance, roadmap/evidence documentation, and GitHub issue intake for the standalone multi-host project.
- `v4.0.0-alpha.40` remains the historical repository-migration checkpoint and final published alpha before stable 4.0.0.

## [4.0.0-alpha.40] - 2026-08-02 (published prerelease)

### Added

- Added a versioned Electron renderer API for connection lifecycle subscriptions, including client-generated stream admission, connection and renderer ownership isolation, overflow reporting, terminal delivery, and explicit unsubscribe cleanup.
- Added deterministic coverage for link loss while the renderer is otherwise idle, partial aggregate cleanup, cancellation and late completion, stale generations, renderer destruction, bounded cancellation ledgers, and retryable remote detach ownership.

### Fixed

- Prevented connection events from pumping before renderer admission and prevented partially failed renderer destruction from leaving a local subscription active after main-process ownership was already detached.
- Made synthetic cleanup terminals deterministic and zero-counted without changing ordinary overflow accounting, while preserving idempotent cleanup retry and prohibiting duplicate native detach.

### Support and evidence boundary

- Alpha.40 adds deterministic Electron lifecycle transport and package proof; it does not add a physical-radio evidence record or promote any backend support label.
- Alpha.40 remains Experimental. Meta Quest and the controllable physical fault-injection peripheral remain deferred to 4.1.

## [4.0.0-alpha.39] - 2026-08-01 (published prerelease)

- Previous 4.0 prerelease. See the preserved detailed history for the complete alpha train.

## Earlier history

The complete detailed pre-stable changelog is preserved byte-for-byte in [`CHANGELOG_HISTORY.md`](https://github.com/sfourdrinier/unified-ble-manager/blob/main/CHANGELOG_HISTORY.md), in addition to the full Git ancestry. It contains the alpha train and inherited project release notes without forcing the new canonical changelog to carry every historical entry inline.
