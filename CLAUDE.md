# CLAUDE.md

This repository contains **Unified BLE Manager 4.x** (`unified-ble-manager`), a host-neutral Bluetooth Low Energy central/GATT package for React Native, Web, Electron, and Node/desktop hosts.

Treat `AGENTS.md`, `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`, `README.md`, and `RELEASE.md` as the current project guidance. Do not infer 4.x behavior from inherited `react-native-ble-plx` 3.x source/docs.

## Package manager and common checks

This repository uses **pnpm** and Corepack.

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

Useful focused commands include:

- `pnpm test:native-protocol`
- `pnpm test:native-protocol:android`
- `pnpm test:native-protocol:apple`
- `pnpm test:native-protocol:winrt`
- `pnpm build:example:web`
- `pnpm build:electron:macos`
- `pnpm build:electron:winrt`
- `pnpm performance:check`

CI owns the broader cross-platform compile/ABI matrix.

## Public architecture

The neutral root exports shared public manager/types and does **not** choose a radio. Consumers use explicit host entrypoints:

- `unified-ble-manager/react-native`
- `unified-ble-manager/web`
- `unified-ble-manager/electron/main`
- `unified-ble-manager/electron/renderer`
- `unified-ble-manager/node/corebluetooth`
- `unified-ble-manager/node/winrt`
- `unified-ble-manager/node/bluez`
- `unified-ble-manager/backend-sdk`
- `unified-ble-manager/testing`
- `unified-ble-manager/codecs`
- `unified-ble-manager/cli`

Profile exports are documented in `README.md` and `docs/PROFILES_AND_COMMANDS.md`.

## 4.x contract invariants

- Public BLE values are `Uint8Array` / `Readonly<Uint8Array>`.
- Base64 is an explicit codec for external protocols, not the native BLE value contract.
- Cancellable public operations use `AbortSignal`; applications do not create public transaction IDs.
- Managers own backend resources and expose asynchronous teardown.
- Connection/GATT/subscription lifetimes are explicit; stale discoveries/handles are not immortal device-object state.
- Capabilities are typed and backend-reported at runtime.
- The root does not silently pick/fallback to a backend.
- Electron main owns the radio/backend; renderers use the versioned IPC client.
- Native/private backend protocols are versioned and fail closed.
- Deterministic backends/mocks are test infrastructure, never production radio fallbacks.

Do not reintroduce the legacy 3.x `BleManager`/`Device`/`Service`/`Characteristic` facade, Base64 public payloads, caller transaction IDs, static `supports()` matrices, or Noble compatibility paths into the 4.x contract.

## Host implementations

### React Native

The React Native host uses the versioned `UnifiedBleProtocolControl` boundary and explicit manager construction. The modernization floor is React Native 0.86+; Expo integration targets SDK 57+. The package contains native code and does not run in Expo Go.

### Web

Web Bluetooth uses its explicit chooser/session integration. Browser user-activation/security restrictions are part of the host contract; do not emulate process-level background scanning/restoration that Web Bluetooth does not provide.

### Electron

Only trusted main-process code selects/owns the radio. Renderer reload/rebind is an ownership/security boundary. Do not load Node-API radio addons in untrusted renderer code.

### Node desktop

First-party desktop backends are owned CoreBluetooth, WinRT, and BlueZ implementations. CoreBluetooth/WinRT addons are built for the exact Node/Electron ABI and architecture that loads them. BlueZ is isolated behind its explicit entrypoint and optional `dbus-next` dependency.

## Evidence and support

Package SemVer and backend support qualification are independent. Stable `4.0.0` stabilizes the documented package/API contract; it does not automatically promote a backend's evidence label.

`docs/generated/PLATFORM_SUPPORT.md` is generated from retained evidence. Compilation, deterministic tests, ABI loading, and mocks prove only those levels and must not be described as live-radio evidence.

## Generated artifacts

Do not hand-edit generated support/reference artifacts, `SBOM.cdx.json`, or `THIRD_PARTY_LICENSES.json` when a generator owns them. Use the repository scripts and verify reproducibility with `pnpm release:artifacts:check` / documentation checks.

## Releases

`main` is the canonical release branch. Follow `RELEASE.md`. Version tags drive `.github/workflows/publish.yml`, which publishes `unified-ble-manager` through npm trusted publishing/OIDC with provenance after the release gates pass.

Never publish a normal release manually, move a published version tag, or weaken evidence/support labels merely to make a stable package release pass.

## Historical fixture names

Some inherited native/example identifiers such as `BlePlxExample` may remain when they are internal fixture/scheme names. Cosmetic native renames are not a release goal; change them only with an explicit compatibility/build rationale and complete validation.
