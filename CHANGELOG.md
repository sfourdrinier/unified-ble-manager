# Changelog

All notable changes to `unified-ble-manager` are documented here.

## [Unreleased]

## [4.0.0] - 2026-08-16

### Stable package and public contract

- Promoted the Unified BLE Manager 4.0 package/API contract to its first stable release.
- Established `sfourdrinier/unified-ble-manager` and `main` as the canonical repository and release branch.
- Preserved the Git ancestry of the 4.0 work while leaving `sfourdrinier/react-native-ble-plx` as the historical and 3.x home.
- Kept platform support qualification independent from package SemVer: stable `4.0.0` does not promote a backend beyond the support label justified by retained evidence.

### Package and host model

- Finalized the host-neutral root plus explicit React Native, Web, Electron, Node/CoreBluetooth, Node/WinRT, Node/BlueZ, backend SDK, testing, codecs, CLI, and profile exports.
- Standardized public BLE data on `Uint8Array`, cancellation on `AbortSignal`, explicit manager ownership, bounded event semantics, typed capabilities, and versioned backend/native protocols.
- Kept React Native, browser, Electron, Node, and third-party backend integration explicit; no production path silently falls back to Noble, Web Bluetooth, or a simulated backend.

### Release integrity

- Migrated canonical CI and release automation from the legacy `master`/`4.0` topology to `main`.
- Stable publication now requires the release tag to identify the exact current `main` commit and reruns package, native-build, ABI, artifact, packed-consumer, and supply-chain checks before npm publication.
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

The complete detailed pre-stable changelog is preserved byte-for-byte in [`CHANGELOG_HISTORY.md`](CHANGELOG_HISTORY.md), in addition to the full Git ancestry. It contains the alpha train and inherited project release notes without forcing the new canonical changelog to carry every historical entry inline.
