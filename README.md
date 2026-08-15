<!-- README.md -->

# Unified BLE Manager

`unified-ble-manager` is a host-neutral Bluetooth Low Energy central/GATT library for React Native, Web, Electron, and Node. It provides one bytes-first manager contract, explicit host integrations, typed backend capabilities, deterministic lifecycle semantics, and first-party desktop backends without silently falling back to a different radio implementation.

**4.0.0 is the first stable release of the Unified BLE Manager package and public API contract.** It is a new package line, not a source-compatible rename of `react-native-ble-plx` 3.x.

**Architecture authority:** [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

> [!IMPORTANT]
> Stable package SemVer and platform support qualification are separate dimensions. `4.0.0` stabilizes the package/API contract; a backend is only Preview, Supported, or Reliability-qualified when the corresponding retained evidence says so. See [`docs/PLATFORMS.md`](docs/PLATFORMS.md) and the generated [platform support report](docs/generated/PLATFORM_SUPPORT.md).

## Install

```sh
pnpm add unified-ble-manager@4.0.0
```

The package is also usable with npm, yarn, or Bun. This repository uses pnpm for reproducible development and release validation.

### Host-specific dependencies

The neutral root does not select or load a radio backend. Import the explicit host entrypoint you need.

For Linux BlueZ, install the optional D-Bus peer in the consuming application:

```sh
pnpm add unified-ble-manager@4.0.0 dbus-next@^0.10.2
```

React Native, Web, macOS CoreBluetooth, and Windows WinRT consumers do not need `dbus-next`.

## Public entrypoints

| Import | Purpose |
| --- | --- |
| `unified-ble-manager` | Host-neutral manager and shared public types; selects no radio. |
| `unified-ble-manager/react-native` | React Native Android/Apple provider and manager construction. |
| `unified-ble-manager/web` | Browser Web Bluetooth provider, chooser, and matched manager session. |
| `unified-ble-manager/electron/main` | Trusted Electron-main backend factories and IPC router/binding. |
| `unified-ble-manager/electron/renderer` | Versioned renderer IPC client; never a radio factory. |
| `unified-ble-manager/tauri` | Tauri v2 webview transport over the shared desktop IPC contract. |
| `unified-ble-manager/node/corebluetooth` | macOS CoreBluetooth Node provider. |
| `unified-ble-manager/node/winrt` | Windows WinRT Node provider. |
| `unified-ble-manager/node/bluez` | Linux BlueZ D-Bus provider. |
| `unified-ble-manager/backend-sdk` | Public backend contract and third-party backend authoring surface. |
| `unified-ble-manager/testing` | Deterministic backend, TCK, and test utilities. |
| `unified-ble-manager/codecs` | Explicit codecs such as Base64 helpers for external protocols. |
| `unified-ble-manager/cli` | Node CLI surface. |

Profile entrypoints are also public: `profiles/commands`, `profiles/standard-commands`, `profiles/heart-rate`, `profiles/battery-service`, `profiles/device-information`, `profiles/health-thermometer`, `profiles/blood-pressure`, and `profiles/ieee-11073`. See [`docs/PROFILES_AND_COMMANDS.md`](docs/PROFILES_AND_COMMANDS.md).

Deep imports are unsupported. Use only documented package exports.

## Core contract

### Bytes first

BLE values are `Uint8Array`. Writes accept `Readonly<Uint8Array>`. The normal public contract does not use Base64; import `unified-ble-manager/codecs` only when an external protocol requires text encoding.

### Cancellation

Cancellable operations accept `AbortSignal`. Application code does not create public transaction IDs; backend operation correlation remains internal.

```ts
const abortController = new AbortController()

await database.write(characteristicPath, new Uint8Array([0x01]), {
  mode: 'with-response',
  signal: abortController.signal,
  deadline: null
})

abortController.abort()
```

### Explicit ownership and cleanup

Managers own scans, connections, GATT databases, subscriptions, and backend resources. `destroy()` is asynchronous and must be awaited when the owning host session ends.

## React Native

Requirements for the React Native host:

- React Native 0.86+
- Expo SDK 57+ when using Expo
- Android min SDK 24
- iOS deployment target 16.4

Create the platform-specific manager explicitly:

```ts
import { Platform } from 'react-native'
import {
  createReactNativeBleManager,
  getNativeUnifiedBleProtocolControl
} from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  platform: Platform.OS === 'ios' ? 'apple' : 'android',
  control: getNativeUnifiedBleProtocolControl(),
  now: () => performance.now(),
  clientId: 'signed-in-user-ble-client',
  managerId: 'main-mobile-ble-manager',
  hostSessionScope: 'com.example.app.mobile-ble'
})

try {
  // Scan, connect, discover, read, write, and subscribe through this manager.
} finally {
  await manager.destroy()
}
```

`hostSessionScope` is a stable host-owned security scope. Do not derive it from a render, request, or operation counter.

### Expo plugin

Add `unified-ble-manager` to the Expo config and build a native development or production app. The package contains native code and does not run in Expo Go.

```json
{
  "expo": {
    "plugins": [
      [
        "unified-ble-manager",
        {
          "isBackgroundEnabled": true,
          "modes": ["central"],
          "neverForLocation": false,
          "bluetoothAlwaysPermission": "Allow $(PRODUCT_NAME) to connect to Bluetooth devices",
          "iosNativeProtocolRestoration": {
            "identifier": "com.example.app.ble",
            "namespace": "com.example.app.ble",
            "epoch": "2026-07-30",
            "clientId": "signed-in-user-ble-client",
            "hostSessionScope": "com.example.app.mobile-ble"
          }
        }
      ]
    ]
  }
}
```

See [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md) for the complete option contract and restoration behavior.

## Web

Use `createNavigatorWebBleManager` from `unified-ble-manager/web` to create a matched chooser/manager session. Browser chooser calls must happen from a transient user activation in a secure Web Bluetooth context.

Web Bluetooth does not provide continuous background scanning or process-level restoration. Destroy the matched manager when its session ends.

See [`docs/WEB.md`](docs/WEB.md).

## Electron

The main process owns the radio and backend. Renderers use the narrow, versioned IPC client and never load a native radio addon directly.

Use:

- `createElectronMainCoreBluetoothBackendProvider` on macOS;
- `createElectronMainWinRtBackendProvider` on Windows;
- `unified-ble-manager/electron/renderer` in untrusted renderer code.

See [`docs/ELECTRON.md`](docs/ELECTRON.md) for ownership, reload/rebind, and IPC requirements.

## Tauri v2

Tauri webviews use `TauriBleIpcTransport` with the official Tauri v2 `invoke`
and `Channel` APIs. The Rust plugin owns the radio; the webview imports no Node
backend or native addon. See [`docs/TAURI.md`](docs/TAURI.md).

## Node and desktop

Node hosts select a backend explicitly:

- `unified-ble-manager/node/corebluetooth` — macOS CoreBluetooth;
- `unified-ble-manager/node/winrt` — Windows WinRT;
- `unified-ble-manager/node/bluez` — Linux BlueZ/D-Bus.

The package ships the CoreBluetooth and WinRT Node-API build sources and the required build tooling, not a universal prebuilt `.node` binary. Native addons must be built for the exact Node or Electron ABI, architecture, and runtime that will load them.

For host Node on macOS:

```sh
pnpm --dir node_modules/unified-ble-manager exec node-gyp rebuild \
  --release \
  --directory native/electron/corebluetooth
```

For Electron, target the exact Electron headers; a Node-ABI build is not interchangeable with an Electron build.

## Platform support and evidence

The package deliberately distinguishes implementation from support qualification. Compilation, deterministic tests, mocks, or an ABI smoke test are valuable evidence, but they are not substitutes for a retained physical-radio run when a support label requires one.

The current authoritative support projection is generated from repository evidence:

- [`docs/PLATFORMS.md`](docs/PLATFORMS.md) — how to interpret support;
- [`docs/generated/PLATFORM_SUPPORT.md`](docs/generated/PLATFORM_SUPPORT.md) — generated support table;
- [`docs/GAPS.4.0.md`](docs/GAPS.4.0.md) — remaining platform and reliability evidence.

A stable `4.0.0` package does not automatically promote any backend support label.

## Development and verification

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

CI additionally exercises platform-specific native build and ABI lanes on Linux, macOS, Windows, Android, Expo CNG, and Apple targets when relevant paths change.

## Examples

- [`example/`](example/) — classic React Native repository fixture.
- [`example-expo/`](example-expo/) — Expo SDK 57 CNG fixture; requires native prebuild/build.
- [`example-electron/`](example-electron/) — deterministic Electron package/IPC fixture, not a live-radio claim.
- [`example-web/`](example-web/) — source-characterization fixture; use `/web` plus [`docs/WEB.md`](docs/WEB.md) for the current public integration.

The examples in this checkout may use `file:..`; consuming applications should validate the installed package artifact separately.

## Migrating from react-native-ble-plx 3.x

4.0 is intentionally a new contract. Do not expect the legacy `BleManager` constructor, Base64 characteristic values, public transaction IDs, static host support helpers, or a compatibility shim.

Read [`MIGRATION_4.0.md`](MIGRATION_4.0.md) before migrating an existing application.

## Project history

Unified BLE Manager evolved from the 4.0 development work that began in [`sfourdrinier/react-native-ble-plx`](https://github.com/sfourdrinier/react-native-ble-plx), itself derived from the broader `react-native-ble-plx` project lineage. The Git commit ancestry is intentionally preserved so authorship, debugging history, and design decisions remain available.

`v4.0.0-alpha.40` was the migration point at which the 4.0 branch and published `unified-ble-manager` package moved into this canonical repository. The old repository remains the historical and 3.x home; 4.x issues, documentation, and releases belong here.

## Release integrity

Releases are built and published by GitHub Actions from version tags. The release workflow verifies the tag/version relationship, package tests, lint/typecheck, native compile/ABI lanes, packed-consumer behavior, generated SBOM and third-party license inventory, and package contents before publishing.

Stable versions publish to npm `latest`; prereleases publish to `next`. Publication uses npm trusted publishing/OIDC with provenance rather than a long-lived npm write token.

See [`RELEASE.md`](RELEASE.md).

## Security

Do not post vulnerability details in a public issue. Follow [`SECURITY.md`](SECURITY.md) for private reporting and the supported security-response path.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`GOVERNANCE.md`](GOVERNANCE.md). Changes to public contracts, backend semantics, capability claims, or support labels require matching tests/evidence rather than documentation-only claims.

## License

Apache License 2.0. See [`LICENSE`](LICENSE) and [`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json).
