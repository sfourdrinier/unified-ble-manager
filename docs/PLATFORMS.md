<!-- docs/PLATFORMS.md -->

# Platform support and evidence

**Architecture authority:** [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

This page is an evidence index, not a static compatibility matrix. An application learns optional behavior from the typed capabilities of its instantiated backend; a platform name, successful build, or compile-time helper never substitutes for an implemented capability and the evidence required for a support claim.

## Package stability and backend support are separate

`unified-ble-manager@4.0.7` is the published **stable package/API** for the 4.x contract; it is immutable. Backend support labels remain evidence-derived and independent of this SemVer. Immutable `4.0.0`, `4.0.1`, `4.0.2`, and `4.0.3` remain published history.

This package is the portable API/semantics freeze; it does **not** mean every first-party backend is
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

`manager.capabilities.supports(id)` means the selected backend implements an
invocable operation; both `supported` and `limited` descriptors satisfy it.
The descriptor returned by `get(id)` preserves evidence and limitations so an
application can require a fully qualified `supported` state when needed.

Host entrypoints select an explicit concrete backend and surface its typed unavailable, permission, adapter, cancellation, deadline, and lifecycle failures.

## Connected-device background monitoring

The Expo Android host offers an explicit connected-device foreground-service
lease. It is absent by default, and `restart: 'never'` is the default. A caller
may update the current notification title/body while holding that lease;
updates do not start a service or acquire another lease. Android preserves the
configured notification channel/icon, connected-device foreground-service
type, ongoing state, and app-launch tap. `while-session-intent-exists` adds
managed boot and package-replacement recovery, but only starts the service
when the native UBM session-intent is present. It never scans or reconnects;
any reconnect policy belongs to the application. After Android has promoted
the recovered service, UBM sends the package-scoped
`com.sfourdrinier.unifiedblemanager.background.FOREGROUND_READY` broadcast.
An app that owns a headless runtime may receive that signal and start its own
work without racing Android's background-service restrictions.

Apple, Web, BlueZ, WinRT, Electron, Tauri, and deterministic backends do not
claim this Android service capability. They reject the Android-only operation
truthfully with `capability.unsupported`; no backend silently ignores an
option or substitutes a supervisor/reconnect loop.

## Peer directory availability

React Native Android is currently the only first-party backend that exposes the
system-bonded directory: `manager.peers.bonded()` enumerates the Android bond
table and `manager.peers.resolve(reference)` rechecks that table before a
reconnect. The reference is backend-owned and opaque; the native address never
becomes a public durable MAC identity. `bonded` means paired metadata, not
reachable or connected. Apps need Android `BLUETOOTH_CONNECT` permission, and a
permission failure is surfaced as `permission.denied` rather than an empty
result.

The other backends retain their truthful boundaries: Web Bluetooth exposes
origin-authorized devices (not bonded devices), while React Native Apple,
CoreBluetooth, BlueZ, WinRT, Electron, and Tauri do not advertise Android
bonded or queued `when-available` support without a native primitive that can
honour it. Their unsupported peer methods fail with `capability.unsupported`.

## React Native notification bursts

React Native Android and Apple both deliver native BLE events through a bounded
native-to-JavaScript ingress queue. Each queue retains at most 512 records or
1 MiB, whichever limit is reached first. This gives applications room for a
peripheral to send a few hundred notifications in a catch-up burst—for example,
about 288 five-minute records covering 24 hours—without turning the queue into
unbounded memory.

Applications should still process notifications promptly and split larger
application-protocol transfers into resumable ranges. If JavaScript cannot drain
the bounded queue, the backend reports `stream.overflow` and closes that ingress
rather than silently losing a prefix. Android and Apple use the same limits;
Web, BlueZ, CoreBluetooth desktop, WinRT, Electron, Tauri, and the deterministic
backend do not pass through this React Native bridge and retain their existing
stream limits and capability reports.

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
