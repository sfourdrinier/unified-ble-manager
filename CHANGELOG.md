# Changelog

All notable changes to `unified-ble-manager` are documented here.

## [Unreleased]

### Added

- BlueZ pairing dispatch: `org.bluez.Device1.Pair`, `org.bluez.Device1.CancelPairing`, and `org.bluez.Adapter1.RemoveDevice` are now allowed through the dbus-next boundary, and the backend registers a just-works (`NoInputNoOutput`) `org.bluez.Agent1` on its own bus (not the system default) so a client-initiated pairing can complete without an external agent (#141, #143).
- `PairOptions.secureConnections` (`'require' | 'prefer' | 'disallow'`, default `'prefer'`): request an LE pairing generation. `'prefer'` defers to the platform. No current backend exposes per-pairing generation selection, so `'require'` and `'disallow'` fail closed with `capability.unsupported` on BlueZ, WinRT, and Android rather than being silently ignored; the contract is in place for a backend that can honour it (#144, #143).

### Fixed

- BlueZ `security.pair()` no longer fires `Device1.Pair` when the operation is aborted or times out while the just-works agent is still registering; it re-checks cancellation after agent registration so a cancelled pairing never proceeds on the daemon (#143).
- BlueZ `security.pair()` reports `paired` (not `cancelled`) when an abort or deadline lands after `Device1.Pair` has already completed the bond, so a bond that was actually created is never reported as if it never happened (#143).

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
