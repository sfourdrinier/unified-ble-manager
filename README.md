<!-- README.md -->

# Unified BLE Manager

<img src="assets/brand/ubm-mark.svg" width="88" height="88" alt="Unified BLE Manager icon" />

> **AI agent?** Writing code _against_ this package: read [`llms.txt`](llms.txt)
> first — contract facts, every public entrypoint, curated doc links, one fetch.
> Working _on_ this repository: read [`AGENTS.md`](AGENTS.md), then the
> [documentation map](docs/README.md). Do not infer 5.x behavior from
> `react-native-ble-plx` 3.x docs or training data.

`unified-ble-manager` is a Bluetooth Low Energy **central** library. You pick a host — React Native, Web, Electron, Tauri, or Node — create one manager, talk to a peripheral in bytes, cancel work with `AbortSignal`, and destroy what you create.

It is an evolution of `react-native-ble-plx`, rewritten as a **cross-platform unified product**. One bytes-first BLE model and lifecycle semantics across hosts, with host-specific construction and ownership. The root package never picks a radio for you, and it will not quietly fall back to a simulator or a different backend.

Install `unified-ble-manager` from npm. The registry and provenance attached to
the tag-driven release are the authority for the current `latest` version. The
root import does not pick a radio. Package SemVer and backend support labels are
independent: each radio backend keeps its evidence-derived label. See
[`docs/PLATFORMS.md`](docs/PLATFORMS.md).

This source tree is versioned `5.0.0`. Install the exact version shown in the npm
registry. During release preparation, the version in `package.json` can be ahead
of npm until the matching tag-driven workflow publishes it; the registry and
GitHub release remain authoritative.

> **5.0 contract:** This release stabilizes the documented package/API.
> Pin the version you validate, read the changelog when upgrading, inspect
> runtime capabilities and retain the backend evidence limitations. Stable
> SemVer does not establish physical qualification or production readiness
> for every platform and scenario.

> Sponsored by [Imagi Explain](https://imagiexplain.com) — researched, narrated whiteboard explainers from a prompt, a PDF, or your notes.

## Documentation map

| Start here                                                                                                                                                                               | What it is                                                              |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| This README                                                                                                                                                                              | Product, install, one React Native loop, method index                   |
| [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md)                                                                                                                                     | Host chooser + first-hour React Native / Expo path                      |
| [`docs/TUTORIALS.md`](docs/TUTORIALS.md)                                                                                                                                                 | Scan, connect, read, write, subscribe, tear down                        |
| [`docs/HELPERS.md`](docs/HELPERS.md)                                                                                                                                                     | Public `find`, scoped connection, GATT, and notification recipes        |
| [`MIGRATION_4.0.28.md`](MIGRATION_4.0.28.md)                                                                                                                                             | Current migration from UBM 4.0.28 to 5.x                                |
| [`docs/WEB.md`](docs/WEB.md) · [`docs/ELECTRON.md`](docs/ELECTRON.md) · [`docs/NODE.md`](docs/NODE.md) · [`docs/TAURI.md`](docs/TAURI.md) · [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md) | Host construction                                                       |
| [`docs/PEERS.md`](docs/PEERS.md)                                                                                                                                                         | Scoped peer directories, persistence, and reconnect-by-reference        |
| [`docs/PROFILES_AND_COMMANDS.md`](docs/PROFILES_AND_COMMANDS.md)                                                                                                                         | Heart Rate, Battery, DIS, and path helpers                              |
| [`docs/NATIVE_ARTIFACTS.md`](docs/NATIVE_ARTIFACTS.md)                                                                                                                                   | Prebuilt native-core identity, refresh, and consumer build rules        |
| [`docs/README.md`](docs/README.md)                                                                                                                                                       | Every document in the repository, with live/historical/generated status |

Writing code with an AI agent? [`llms.txt`](llms.txt) is the machine-readable
package overview — contract facts, every public entrypoint, and curated doc
links in one fetch. Agents contributing to this repository start at
[`AGENTS.md`](AGENTS.md) and the [documentation map](docs/README.md).

## Install

```sh
pnpm add unified-ble-manager@5.0.0
```

Installable with npm, yarn, or Bun. This repository uses pnpm. Bun 1.4.2 loads the same desktop Node-API addon as Node. `bun scripts/ci/bun-desktop-host-smoke.js` opens the synthetic central and closes it. `--list-adapters` lists the OS adapters; on glibc Linux x64 it returned both host adapters. `scripts/ci/bun-desktop-h10-session.js` is an opt-in session for a simulator named exactly `SIM Polar H10 0001`. On macOS Apple Silicon that session scanned, connected, read, wrote, notified, and disconnected. On glibc Linux x64 the same GATT session completed and `setEventWaker` ran. Against the installed `5.87-ubm.4` daemon, disconnect after discovery returned `lease-released-protected` and left the link up, as Node does on that stack. Source `5.87-ubm.6` ends a finished read or write hold when the call completes. That daemon was installed and the same H10 session then returned `lease-released-indeterminate` and left the link up, because profile-probe auto-connect bookkeeping was recorded as an unknown holder. Source `5.87-ubm.7` does not treat that bookkeeping as a hold on an exclusive link this process created, and that exclusive release stops kernel auto-connect for an untrusted device. Installing it changed the same H10 disconnect to `lease-released-protected` and left the link up: the controller had already initiated the bonded link, and the lease adopted it as borrowed. Source `5.87-ubm.8` releases a locally initiated link when no other application hold remains. Installing it, the same H10 session reported disconnect `released` and close `released`, and the link was down. Source `5.87-ubm.9` leaves an unbonded link up when an encrypted attribute returns Insufficient Encryption. Installing it, the unbonded Linux session, the macOS session, and two back-to-back Windows sessions on source digest `0b31ce8e` each completed the same exchange and left the link down. Neither the smoke nor those receipts is a platform evidence label.

Node and Electron on macOS, Windows and Linux use the shared Rust core,
shipped prebuilt in the package: macOS desktop support is Apple Silicon (`arm64`) only;
Windows and Linux desktop support includes `arm64` and `x64`.
Intel macOS desktop is outside the UBM support policy, not a statement about
Apple's support for particular macOS versions. Nothing compiles on install, no Rust toolchain is needed, and
no other package is required (Linux no longer needs `dbus-next`). Linux needs
glibc 2.35+ and `libdbus-1.so.3`; see [`docs/NODE.md`](docs/NODE.md) for the
runtime requirements and load errors.

React Native iOS and Android consume the prebuilt Rust core shipped in the
package by default: no Rust toolchain is needed. `UBM_NATIVE_BUILD` accepts
only unset/empty or `prebuilt` (default) and `source` (contributors building
the Rust core themselves; see `CONTRIBUTING.md`); any other value fails
`pod install` and the Gradle build.

The packaged native core and the JavaScript package are one sealed release
unit. Consumer builds verify the native build identity before use and fail
with `protocol.incompatible` when the linked artifact does not match. For
prebuilt/source modes and the exact refresh rules for repository fixtures, see
[`docs/NATIVE_ARTIFACTS.md`](docs/NATIVE_ARTIFACTS.md).

Every React Native and Expo factory runs that core through the
`UnifiedBleRustCore` TurboModule: one process-owned Rust owner per app, one
session lease per manager. There is no TypeScript or protocol-control route and
no option to request one. Before its first radio call the factory checks the
binary's build identity, contract revision and wire revision against the
identity this package was sealed with, and fails `protocol.incompatible` on any
difference. Platform events reach JavaScript through one wake-driven drain, so
an idle manager makes no bridge calls. The wire is described in
[`docs/MOBILE_RUST_WIRE.md`](docs/MOBILE_RUST_WIRE.md).

## Public entrypoints

The root import selects no radio. Import the host you actually run.

| Import                                   | Purpose                                                                        |
| ---------------------------------------- | ------------------------------------------------------------------------------ |
| `unified-ble-manager`                    | Host-neutral manager, handles, helpers, and shared types                       |
| `unified-ble-manager/react-native`       | React Native Android / Apple manager                                           |
| `unified-ble-manager/react`              | React provider, hooks, and React-facing type utilities                         |
| `unified-ble-manager/expo`               | Expo development-build manager, readiness, and native configuration checks     |
| `unified-ble-manager/web`                | Web Bluetooth chooser + matched manager                                        |
| `unified-ble-manager/electron/main`      | Trusted Electron-main radio + IPC router                                       |
| `unified-ble-manager/electron/renderer`  | Public `BleManager` factory over an authenticated IPC transport; never a radio |
| `unified-ble-manager/tauri`              | Tauri v2 zero-plumbing `BleManager` factory                                    |
| `unified-ble-manager/node/corebluetooth` | macOS Node provider (shared Rust core over CoreBluetooth)                      |
| `unified-ble-manager/node/winrt`         | Windows Node provider (shared Rust core over WinRT)                            |
| `unified-ble-manager/node/bluez`         | Linux Node provider (shared Rust core over BlueZ)                              |
| `unified-ble-manager/backend-sdk`        | Backend authoring contract                                                     |
| `unified-ble-manager/advanced`           | Expert UUID/provider utilities; no radio or restoration identity forge         |
| `unified-ble-manager/testing`            | Deterministic backend and TCK utilities                                        |
| `unified-ble-manager/codecs`             | Byte/`DataView` helpers and IEEE-11073 numbers — not Base64                    |
| `unified-ble-manager/cli`                | Node CLI                                                                       |

Profile subpaths: `profiles/commands`, `profiles/standard-commands`, `profiles/heart-rate`, `profiles/battery-service`, `profiles/device-information`, `profiles/health-thermometer`, `profiles/blood-pressure`, `profiles/ieee-11073`.

Deep imports are unsupported.

Linux consumers require the explicit daemon mechanisms documented in
[`docs/BLUEZ_LE_GATT.md`](docs/BLUEZ_LE_GATT.md). Installing this package does
not install or replace the system Bluetooth daemon; stock aggregate discovery
signals are not a substitute for authoritative LE GATT readiness.

## React provider and hooks

`unified-ble-manager/react` supplies the provider and hooks; create the manager
with the explicit host entrypoint for the application (`react-native`, `expo`,
or another supported host). It does not select or load a radio backend.

## Create a React Native manager

Requirements: React Native 0.86+, Expo SDK 57+ when using Expo, Android min SDK 24, iOS 16.4. The package contains native code and does not run in Expo Go.

```ts
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  instanceId: 'main'
})
```

On Android 12+ the app must request `BLUETOOTH_SCAN` and `BLUETOOTH_CONNECT` itself. The library does not call `PermissionsAndroid`.

On Expo, follow `manager.readiness()` actions: `manager.permissions.request({ purpose: 'scan-and-connect' })` shows the system Bluetooth prompt on Android and on Apple (iOS/tvOS, on request — reading readiness never prompts) and reports `{ requested, granted, denied, recommendedSettingsTarget }`. See [`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md) for the prompt, restriction, timeout, and restoration semantics.

AccessorySetupKit-configured iOS apps use separate accessory authorization:
while global authorization is `notDetermined`, a global permission request is
unsupported and must not replace the user-initiated `manager.choose` flow.
An accessory grant is not a global Bluetooth grant; see the Expo guide before
copying the ordinary global-permission recipe.

On Android, `manager.peers.bonded()` lists paired system peers and
`manager.peers.resolve(reference)` rechecks a saved reference before
`manager.connect(peer, { intent: 'when-available' })`; paired does not mean
reachable. See [`docs/PEERS.md`](docs/PEERS.md) for the persistence and error
semantics.

### Recovering after an adapter interruption

Adapter power and authorization changes are observable through the same public
contract on every backend:

```ts
const watch = await manager.adapter.watchState()
let previous = watch.initial
for await (const item of watch.values) {
  if (item.kind !== 'value') continue
  const state = item.value
  const restored = previous.power !== 'on' && state.power === 'on'
  previous = state
  if (restored && state.availability === 'available' && state.authorization === 'granted') {
    // Reconcile your saved peer/reference and reconnect using your product policy.
  }
}
await watch.stop()
```

UBM invalidates affected connections when the adapter is unavailable and does
not reconnect silently. The host observes `watchState()`, resolves a fresh
peer/reference, and starts a new connection generation. Do not poll, create a
second manager, or reuse old GATT/database handles.

### Expo plugin

Use an Expo development build, never Expo Go. Plugin options live in
[`docs/EXPO_PLUGIN.md`](docs/EXPO_PLUGIN.md).

For an Expo development build, use the Expo host factory and inspect readiness
before starting a user action:

```ts
import { createExpoBleManager } from 'unified-ble-manager/expo'

const ble = await createExpoBleManager()
const readiness = await ble.readiness()
// Android only: system UI association, not bonding or an active connection.
// Association alone wakes nothing: arm presence (ble.presence.observe) as in
// docs/BACKGROUND.md before the app can be woken for a known peer.
const associated = await ble.association.associate({ name: 'Sensor' })
```

### Packed Expo / Tauri export proof

The packed-host gate (`pnpm prepack && node scripts/ci/packed-host-consumer-check.js`)
installs the generated tarball into an isolated consumer and proves the
conditional `./expo`, `./react`, and `./tauri` exports: CJS and ESM runtime
imports/loadability, plus TypeScript imports under Bundler and NodeNext
resolution. It is an exact packed export/type/import proof, not a full Expo
application build.

The `example-expo` source-tree CNG prebuild and Android debug APK/assembly are
separate source/plugin and Android compile evidence. Apple/Xcode, EAS builds,
and physical-device permissions, restoration, background behavior, and radio
reliability each require their own successful host- or device-specific proof.

## One complete loop

Values are `Uint8Array`. Cancellable work takes `AbortSignal` and bounded
operations use `timeoutMs`. Advertised names live on `localName`, not
`device.name`.

```ts
// @ubm-recipe finite-hrs
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import { BATTERY_LEVEL_CHARACTERISTIC, parseBatteryLevel } from 'unified-ble-manager/profiles/battery-service'

const manager = await createReactNativeBleManager()
const abort = new AbortController()

try {
  const peer = await manager.find({
    query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] },
    timeoutMs: 10_000,
    signal: abort.signal,
    select: 'first'
  })
  await manager.withDiscoveredConnection(peer, { timeoutMs: 15_000, signal: abort.signal }, async ({ gatt }) => {
    const battery = gatt.characteristic('180F', BATTERY_LEVEL_CHARACTERISTIC, {
      serviceOccurrence: 0,
      characteristicOccurrence: 0
    })
    const bytes = await battery.read({ timeoutMs: 10_000, signal: abort.signal })
    consume(parseBatteryLevel(bytes))
  })
} finally {
  await manager.destroy()
}
```

Battery Level and Heart Rate Control Point are optional or conditional; see [`docs/TUTORIALS.md`](docs/TUTORIALS.md). Persistent subscriptions also live there.

Web Bluetooth replaces the scan with `ble.choose(...)` from a user gesture. React Native and Expo can explicitly use that same public chooser with Android CDM or eligible iOS AccessorySetupKit, then connect the selection through the same manager; setup is not a scan or a relaunch receipt. Native eligibility, declarations and refusals are in [Getting Started](docs/GETTING_STARTED.md#optional-native-system-chooser). The [complete Web Bluetooth guide](docs/WEB.md) and [TypeScript/Vite example](example-web/) cover browser chooser permissions, authorized peers, bounded operations, notifications, structured errors, and safe cleanup. Tauri and the Electron renderer use different host entrypoints — see those host pages.

## Why the API looks like this

| Shape                                 | Benefit                                                                      |
| ------------------------------------- | ---------------------------------------------------------------------------- |
| `Uint8Array`, not Base64              | BLE is binary. Encode text at the HTTP boundary yourself.                    |
| `AbortSignal` + `timeoutMs`           | Cancel the way you cancel `fetch`. The library owns operation correlation.   |
| Observation → `Connection` → snapshot | A peer id is not a live link. After disconnect, old objects would lie.       |
| Paths from `snapshot()`               | The same UUID can appear twice. Generations make stale handles fail closed.  |
| Verbose scan `delivery`               | Overflow is visible. A second scan is `scan.already-active` unless you join. |
| Explicit host import                  | A failed native backend must not become Web Bluetooth or a mock.             |
| Await `destroy()`                     | The radio and every lease have an owner. Fire-and-forget leaks them.         |

## Method index

### `BleManager`

| Member                                         | Use                                                  |
| ---------------------------------------------- | ---------------------------------------------------- |
| `scan(options)`                                | Start a bounded `ScanSession`. You must `stop()` it. |
| `find(options)`                                | Find one normalized `BlePeer` and stop its scan.     |
| `choose(options)`                              | Use a system chooser where the backend supports it.  |
| `connect(peer, { signal, timeoutMs })`         | Open a connection lease.                             |
| `destroy()`                                    | Async teardown. Await it. Inspect `CleanupRecord`.   |
| `adapter.state()` / `adapter.waitUntilReady()` | Readiness of this instantiated backend.              |
| `capabilities` / `discovery`                   | Runtime feature and discovery truth from the host.   |

### `ScanSession`

| Member         | Use                                                                                                                                                  |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `observations` | Bounded stream: `value`, `overflow`, or `terminal`. Drop-policy overflow means ads were missed; the scan stays up. `error` fail-closes.              |
| `events`       | Optional derived current-view events: `observed` and monotonic `lost` (`observation.reportLostAfterMs`); unsupported host façades reject this option |
| `plan`         | Host-owned native/residual planning diagnostics, or `null` when this host has no planner                                                             |
| `stop()`       | End the scan and return a cleanup receipt. `find` already does this.                                                                                 |

`AdvertisementObservation.device` is identity (`id`, address, stability). The advertised name is `observation.localName`.

`scan({ observation: { reportLostAfterMs } })` derives timeout events from the same coalesced current view. The timeout is monotonic and bounded; RF absence, OS throttling, filtering, process suspension, or a stopped scan can all produce a derived `lost` event. Raw advertisement inclusion is capability-gated and unsupported by the normal public façade. Typed `platform` controls are validated at the public boundary; controls not implemented by the selected host reject before radio work rather than silently no-op.

### `Connection`

| Member                                                   | Use                                                                                                                   |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `discover({ signal, timeoutMs })`                        | Discover GATT and return a generation-bound database                                                                  |
| `release()`                                              | Drop the lease (happy-path cleanup)                                                                                   |
| `disconnect()`                                           | Ask the radio to disconnect                                                                                           |
| `connection.controls.readRssi(options)`                  | RSSI when the instantiated backend advertises `connection:rssi`                                                       |
| `connection.controls.requestMtu(n, options)`             | Request an ATT MTU when `connection:request-mtu` is advertised; inspect the returned observation                      |
| `connection.controls.maximumWriteLength(mode)`           | Authoritative mode-specific write limit when `gatt:maximum-write-length` is advertised                                |
| `connection.controls.writeReadiness('without-response')` | Bounded readiness only when `gatt:write-without-response-readiness` is advertised; otherwise `capability.unsupported` |
| `events`                                                 | Lifecycle stream for this generation                                                                                  |

Controls report the truth of the instantiated host backend, including
`supported`, `limited`, `unavailable`, or `unsupported`; host family alone is
not evidence of support. `manager.capabilities.supports(id)` answers whether
the operation is implemented and invocable, so it returns `true` for both
`supported` and `limited`. Use `manager.capabilities.get(id)` when application
policy needs to distinguish full qualification from a named limitation.
Readiness is unsupported until the backend advertises
the readiness capability, and a readiness event does not prove a later payload
was retained.

Runtime capability truth for each host is in the [semantics host matrix](docs/UNIFIED_SEMANTICS.md#172-current-pr8-host-matrix): read it
before relying on MTU or PHY controls. Per-platform derivation limits apply —
on Android the effective MTU is unavailable before a successful MTU exchange,
on Apple there is no caller-directed MTU request (the effective MTU is
derived per link), and on BlueZ a withheld link answers `capability.unavailable`.
In
particular, React Native Android exposes MTU request/effective observation and
PHY read/request as `limited` / deterministic controls: effective MTU is
unavailable before a successful `onMtuChanged` callback, and PHY request
`accepted` plus its observation come from the native callback result. Direct
CoreBluetooth Node/Electron-main readiness is also `limited` / deterministic
when both native readiness hooks are bridged. React Native Apple reports the
same readiness as `limited` (`canSendWriteWithoutResponse` plus
`peripheralIsReady(toSendWriteWithoutResponse:)`). Windows 11 build 22000
desktop, Electron, and Tauri observe connection parameters (limited,
`winrt-connection-parameters-22000`). BlueZ, CoreBluetooth, and older Windows
do not. `subrate` and `connection:subrate` remain unsupported.
`writeWhenReady` is available only when the instantiated backend advertises
authoritative write-without-response readiness; otherwise it rejects
`capability.unsupported` (or `capability.unavailable` when the registered
capability cannot currently be used). It accepts only `{ signal, timeoutMs }`,
waits at the connection FIFO head, rechecks the generation-bound database path
and readiness stream before dispatch, and never replays an uncertain write.
Cancellation and teardown retain readiness cleanup failures for the manager's
cleanup receipt. The separate
`writeReadiness('without-response')` stream is an observation surface, not an
automatic write helper.

### `GattDatabase`

| Member                                                                        | Use                                                                              |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `snapshot()`                                                                  | Immutable services / characteristics / descriptors                               |
| `service(uuid, selector?)`                                                    | Generation-bound `GattService` object                                            |
| `characteristic(serviceUuid, characteristicUuid, selector?)`                  | Generation-bound `GattCharacteristic` object                                     |
| `characteristic.read(options)`                                                | `Uint8Array`                                                                     |
| `characteristic.write(value, { response, signal, timeoutMs })`                | `response` is `'required'`, `'not-required'`, or `'automatic'`                   |
| `characteristic.writeWhenReady(value, { signal, timeoutMs })`                 | Bounded write-without-response helper when authoritative readiness is advertised |
| `characteristic.writeLong(value, { response, signal, timeoutMs, chunkSize })` | Chunked write when supported                                                     |
| `characteristic.subscribe({ signal, timeoutMs, stream })`                     | Notification / indication stream                                                 |
| `characteristic.descriptor(uuid).read/write(...)`                             | Descriptor bytes through the generation-bound characteristic object              |

Use the generation-bound service and characteristic objects returned by the
public database. Do not manufacture advanced portable paths or retain objects
after disconnect, service change, or rediscovery.

### `Subscription`

| Member              | Use                                                                                                                      |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `requestedDelivery` | The caller's preference or requirement, if supplied.                                                                     |
| `effectiveDelivery` | The settled host observation: `notification`, `indication`, or `unknown` when the platform does not report it.           |
| `values`            | Bounded stream of `value` / `overflow` / `terminal` items. A value item carries bytes, delivery, timestamp, and sequence |
| `remove()`          | Always, including after abort; inspect the cleanup receipt                                                               |

### Scoped façade methods

| Helper                       | Use                                             |
| ---------------------------- | ----------------------------------------------- |
| `withConnection`             | Run a function and always `release()` the lease |
| `withDiscoveredConnection`   | Connect, discover, run, then `release()`        |
| `withScan`                   | Start a scan, run a function, then stop it      |
| `connection.lifecycleEvents` | Observe generation-bound lifecycle transitions  |

### Host factories

| Factory                                                                                | Returns                                                                                                                         |
| -------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `createReactNativeBleManager`                                                          | Zero-plumbing public React Native manager                                                                                       |
| `createReactNativeBleManagerWithEnvironment`                                           | Injectable RN factory for tests                                                                                                 |
| `createExpoBleManager`                                                                 | Expo development-build manager with readiness and native configuration checks                                                   |
| `createWebBleManager`                                                                  | Zero-plumbing public Web manager; use `ble.choose()` from a user gesture                                                        |
| `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`    | One-call Node managers                                                                                                          |
| `createElectronMainCoreBluetoothBackendProvider` / `WinRt`                             | Main-process provider; you still build a `BleManager`                                                                           |
| `createElectronRendererBleManager` / `createElectronRendererBleManagerWithEnvironment` | Public renderer `BleManager` over a preload transport; the renderer never loads a radio backend                                 |
| `createTauriBleManager`                                                                | Zero-plumbing Tauri `BleManager`; tests use `createTauriBleManagerWithEnvironment`                                              |
| `createBleManagerFromProvider`                                                         | Advanced provider construction                                                                                                  |
| `createPublicBleManagerFacade`                                                         | Projects an already-owned `/advanced` manager into the root public `BleManager`; creates no advanced manager, backend, or radio |

## Other hosts

- **Web:** user-gesture `ble.choose()`, then the same `connect` / GATT handles. No continuous scan. [`docs/WEB.md`](docs/WEB.md)
- **Electron:** main owns the radio; the renderer creates the public manager from its authenticated preload transport. [`docs/ELECTRON.md`](docs/ELECTRON.md)
- **Node:** `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`, or list adapters and `createBleManagerFromProvider`. Published releases ship the Node-API desktop-core prebuild for macOS Apple Silicon (`arm64`) and Windows/Linux `arm64`/`x64`. [`docs/NODE.md`](docs/NODE.md)
- **Tauri:** `createTauriBleManager()` returns the public `BleManager`; test transports use `createTauriBleManagerWithEnvironment`. [`docs/TAURI.md`](docs/TAURI.md)

`5.0.0` publishes to npm `latest`; after publication, a bare install selects
the stable 5.0 line. Numbered 5.x RCs use `next`. Publication uses npm trusted
publishing/OIDC with provenance.

## Migrating from react-native-ble-plx

This is a rewrite, not a rename. There is no drop-in BleManager constructor, no Base64 characteristic values, no public transaction IDs, and no compatibility shim.

Apps already using UBM should read [`MIGRATION_4.0.28.md`](MIGRATION_4.0.28.md).
The [`ble-plx migration record`](MIGRATION_4.0.md) is historical, non-copyable
comparison material, not current installation or restoration guidance.

## Examples

- [`example/`](example/) — classic React Native fixture (`file:..`).
- [`example-expo/`](example-expo/) — Expo SDK 57 CNG fixture; requires a native prebuild.
- [`example-electron/`](example-electron/) — deterministic package/IPC smoke, not a live-radio claim.
- [`example-web/`](example-web/) — Chrome + physical Heart Rate Service harness.
- [`example-tauri/`](example-tauri/) — Tauri v2 public-manager proof.

## Development

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
```

## Maintainers

Contract, evidence, and release process live in [Current 5.0 authority](docs/README.md#current-50-authority), [`docs/PLATFORMS.md`](docs/PLATFORMS.md), [`RELEASE.md`](RELEASE.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`GOVERNANCE.md`](GOVERNANCE.md), [`SECURITY.md`](SECURITY.md), and [`SUPPORT.md`](SUPPORT.md).

## License

New UBM 5.0 material is made available under the UBM Source Available License 1.0
(`LicenseRef-UBM-Source-Available-1.0`), a commercial source-available license —
not an OSI-approved open-source license. See
[`LICENSE-UBM-SOURCE-AVAILABLE-1.0.md`](LICENSE-UBM-SOURCE-AVAILABLE-1.0.md) and
[`NOTICE`](NOTICE).

The top-level [`LICENSE`](LICENSE) is this UBM license. Material inherited from the
4.x Apache baseline stays under its Apache License 2.0 grant; its text is in
[`LICENSES/Apache-2.0.txt`](LICENSES/Apache-2.0.txt). Existing rights are unaffected. New contributions
follow the assent path in [`CONTRIBUTING.md`](CONTRIBUTING.md). Third-party material
is listed in [`THIRD_PARTY_LICENSES.json`](THIRD_PARTY_LICENSES.json).
