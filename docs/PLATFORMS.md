<!-- docs/PLATFORMS.md -->

# Platform support and evidence

**Architecture authority:** [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

This page is an evidence index, not a static compatibility matrix. An application learns optional behavior from the typed capabilities of its instantiated backend; a platform name, successful build, or compile-time helper never substitutes for an implemented capability and the evidence required for a support claim.

## Package stability and backend support are separate

`unified-ble-manager@4.0.0-rc.3` is the prepared **release-candidate package/API** for the 4.x contract; publication remains pending the RC3 tag workflow. Stable
`4.0.0` is reserved for the post-PR12 release gate.

This release candidate is the portable API/semantics freeze candidate; it does **not** mean every first-party backend is
automatically Preview, Supported, or Reliability-qualified.

Backend support labels remain derived from retained evidence. A backend can therefore be Experimental while the host-neutral package is stable. Conversely, deterministic, compile, ABI, or package proof cannot be relabeled as live-radio evidence merely because the package version no longer has a prerelease suffix.

[`generated/PLATFORM_SUPPORT.md`](generated/PLATFORM_SUPPORT.md) is the generated evidence projection for the current package. Its generator validates versioned records beneath `evidence/v1/records/`, binds claims to the exact artifact where the evidence model requires it, and fails validation when the generated page is stale.

This document deliberately does not duplicate that generated table as a manually maintained matrix.

## Evidence labels

`Experimental`, `Preview`, `Live Preview`, `Supported`, and `Reliability-qualified` have the exact meanings defined in [`evidence/v1/README.md`](../evidence/v1/README.md).

A lower proof level must remain visible as a limitation. Hardware unavailability blocks the affected support label; it does not erase valid deterministic/TCK/package/build evidence and it does not force a stable host-neutral API contract back into prerelease SemVer.

WinRT compilation or ABI loading, for example, is not by itself a Windows live-radio claim. The same rule applies across Android, Apple/CoreBluetooth, BlueZ, Web Bluetooth, and Electron host boundaries.

Meta Quest and the controllable nRF52840 fault-injection controller remain deferred to 4.1. Neither is a 4.0 entrypoint or a requirement for the 4.0.0 package/API release.

## Runtime capability truth

The public core consumes the versioned backend contract and uses backend-reported capabilities at runtime. It has no static platform-support matrix, public Base64 BLE payload path, legacy `BlePort`/`PortBleManager` compatibility surface, or production Noble fallback.

Host entrypoints select an explicit concrete backend and surface its typed unavailable, permission, adapter, cancellation, deadline, and lifecycle failures.

Deterministic and mock boundaries are test-only. They prove contract/fault behavior, not live radio. Package, compile, ABI, and export checks prove only the level they actually exercise. Native compilation and package installation do not promote a backend to a higher support label.

## Evidence records

[`GAPS.4.0.md`](GAPS.4.0.md) inventories current evidence work. The generated support page consumes versioned evidence manifests containing backend identity, protocol versions, package digest where applicable, OS/runtime/hardware, commands, result artifacts, limitations, revalidation rules, and responsible maintainer.

The host guides describe the packed 4.0 contract and its proof boundaries; they do not replace runtime capability or generated evidence inspection:

- [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md)
- [`BACKGROUND.md`](BACKGROUND.md)
- [`WEB.md`](WEB.md)
- [`ELECTRON.md`](ELECTRON.md)
- [`NODE.md`](NODE.md)
- [`TVOS.md`](TVOS.md)

## Related records

- [`../ROADMAP.4.0.md`](../ROADMAP.4.0.md)
- [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
- [`GAPS.4.0.md`](GAPS.4.0.md)
- [`../RELEASE.md`](../RELEASE.md)
