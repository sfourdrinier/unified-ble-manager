<!-- docs/NODE.md -->

# Node.js

The root import does not open an adapter. Pick the entrypoint for your OS:

| Import                                   | Host    | Radio underneath                    |
| ---------------------------------------- | ------- | ----------------------------------- |
| `unified-ble-manager/node/corebluetooth` | macOS   | shared Rust core over CoreBluetooth |
| `unified-ble-manager/node/winrt`         | Windows | shared Rust core over WinRT         |
| `unified-ble-manager/node/bluez`         | Linux   | shared Rust core over BlueZ         |

All three execute one shared Rust core (`DesktopCentral` in `crates/ubm-desktop`, btleplug plus narrow OS adapters) through one N-API addon. This source targets `5.0.0`. Tagged releases ship the addon prebuilt for `linux-x64`, `linux-arm64`, `darwin-arm64`, `darwin-x64`, `win32-x64` and `win32-arm64`, under `native/desktop-core/prebuilds/<platform>-<arch>/`. A normal install compiles nothing and needs no Rust toolchain. The app no longer needs `dbus-next` on Linux.

Runtime requirements:

- **Linux:** glibc 2.35 or newer, and `libdbus-1.so.3` (package `libdbus-1-3`). musl is not supported: loading fails with `no-prebuilt-for-target`, and the detected libc is in the error. A `dlopen` failure, such as a missing `libdbus`, is reported verbatim.
- **macOS:** the process must hold Bluetooth permission (TCC). A process without it is terminated by macOS when the radio opens. That is a macOS policy, not a library failure.
- **Windows:** Windows 10 1809 or newer with a Bluetooth LE adapter.

## One-call factories

```ts
import { createCoreBluetoothBleManager } from 'unified-ble-manager/node/corebluetooth'
import { createWinRtBleManager } from 'unified-ble-manager/node/winrt'
import { createBluezBleManager } from 'unified-ble-manager/node/bluez'

const manager = await createCoreBluetoothBleManager()
```

Backend and adapter ids are the 4.x ones: backend ids `unified-ble:corebluetooth`, `unified-ble:winrt` and `unified-ble:bluez-dbus` (`COREBLUETOOTH_BACKEND_ID`, `WINRT_BACKEND_ID`, `BLUEZ_BACKEND_ID`), and adapter ids `corebluetooth-default-adapter`, the BlueZ object path (`/org/bluez/hci0`) and the raw Windows adapter device id. An adapter id depends only on that adapter, so a persisted `adapterId` keeps working when other adapters come and go.

If there is no adapter, the factory throws `adapter.unavailable`. If more than one adapter exists and you omit `adapterId`, the factory picks the first one in a deterministic order (by adapter id), so a single-adapter host needs no configuration and a multi-adapter host selects the same controller every run. `provider.listAdapters()` lists every OS adapter. An adapter the OS could not describe is still listed, as `unavailable` with the OS's reason. Pass `adapterId` to target a specific controller.

On `SIGINT`/`SIGTERM`, await `manager.destroy()`. Then scan/connect/GATT with the same `BleManager` helpers as React Native.

### Load and identity failures

The addon is found only from the installed package's own location: never through the process cwd, never from another platform's or architecture's binary, and never through a TypeScript fallback. Before any radio call, the host checks the binary's `nativeBuildIdentity()` against the identity the package was sealed with: contract revision, source digest, binding schema, target, and release profile.

| Cause                                                               | Error                                                                                       |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| No prebuild for this platform/arch/libc                             | `capability.unavailable` · `<host>.native-boundary.load` · `no-prebuilt-for-target`         |
| The OS refused to load the file                                     | `capability.unavailable` · `<host>.native-boundary.load` · `load-failed` (dlopen text kept) |
| File does not match its identity sidecar, or the sidecar is missing | `protocol.incompatible` · `<host>.native-boundary.version`                                  |
| Binary identity differs from the package                            | `protocol.incompatible` · `<host>.native-boundary.version` (the differing fields are named) |
| `UBM_NAPI_ADDON` is not an absolute path                            | `argument.invalid` · `<host>.native-boundary.load`                                          |
| Factory called on the wrong OS                                      | `capability.unavailable` · `<host>.native-boundary.load` · `{linux,macos,windows}-required` |

`<host>` is the 4.x operation prefix of each OS: `direct-gatt` (CoreBluetooth), `winrt` or `bluez`.

### Scan observations

As in 4.x, a scan reports only what the radio saw while that scan ran. The core queues sightings only while a scan is live and clears the queue when a scan starts or stops, and the provider refuses (with a `scan-observation-foreign` diagnostic) any observation the core queued for another scan. Each observation's `receivedAtMonotonicMs` is when the core received it, on the host's clock, not when the host took it. `provenance` says what it is: the OS's merged device state (BlueZ `Device1`, a known-device report) is `platform-derived`; a single advertisement's own data is `platform-raw` on WinRT and `platform-derived` on CoreBluetooth (a parsed advertisement dictionary) and BlueZ, as the 4.x backends labelled them.

### Error operation ids

Every public error reports the 4.x operation id of its host and operation, as the TypeScript backends did: `direct-gatt.connect`, `winrt.gatt.read`, `bluez.scan.start`, `direct-gatt.gatt.database-read` (a database handle's read on CoreBluetooth and WinRT; BlueZ handles reported `bluez.gatt.read`), `winrt.security.pair`, `bluez.provider.select-adapter`, and so on. The core's own operation for the failure (for example `gatt.read` or `discovery.database-bound`) never becomes the public id. When the error has no OS detail it is kept for diagnosis in `platform.metadata.coreOperation` of the `desktop-rust-core` / `core-detail` detail.

Source mode, for contributors only: `UBM_NAPI_ADDON=/absolute/path/to/ubm_echo.<platform>-<arch>.node` loads that checkout build exclusively (built by `node scripts/ci/build-napi-addon.js`). Digests are still checked. Only the release-profile rule is relaxed.

### BlueZ bus and pairing generation

`createBluezBleManager`, `createDbusNextBluezBackendProvider` and Electron main's `createElectronMainBluezBackendProvider` take the same two options.

`busKind` reaches the core as the central's D-Bus bus. `'system'` is the default. `'session'` serves a BlueZ exported on the session bus (mock or sandboxed daemons). A build that cannot reach that bus answers `capability.unsupported`; it never falls back silently to the system bus. Any other value is `argument.invalid`.

`pairingGeneration` (a host-supplied `BluezPairingGenerationController`) is carried to the core. A pair that directs `secureConnections: 'require' | 'disallow'` then holds the adapter-wide generation for the ceremony, and the core restores the previous value afterwards. Without a controller, a directed generation is `capability.unsupported`. The package never escalates on its own. While the generation is held, the setting applies to every pairing on that adapter.

### Adapter state, admission and adapter loss

The adapter snapshot reports what the OS says, in the 4.x vocabulary. CoreBluetooth `resetting` is power `resetting`. `unsupported` is availability and power `unsupported`, with authorization `unavailable`. `unauthorized` is authorization `denied`.

A CoreBluetooth central waits up to 10 s for its first usable state (powered on, not refused) before listing or creating succeeds. Otherwise it fails with `capability.unavailable`, platform code `adapter-initialization-timed-out`, as 4.x did.

Radio work is refused before any radio effect, with the 4.x per-OS admission:

- **CoreBluetooth** checks authorization first (`permission.denied`, `permission.restricted`, `permission.not-determined`), then availability (`adapter.unavailable`), then power (`adapter.powered-off`, `adapter.resetting`).
- **WinRT** checks availability first, then authorization, then power.
- **BlueZ** refuses only on lifecycle, as the dbus-next backend did.

An adapter loss is a power-off, resetting, unsupported, revoked authorization, a removed adapter, or a restarted bluetoothd. It tears down everything live. In-flight operations settle `operation.reset`. Scans and subscriptions end `source-failed`. Links are released: CoreBluetooth and WinRT emit `connection-state-changed` with reason `adapter`, while BlueZ invalidates them without an event. The backend generation advances (`1`, `2`, … as the 4.x backends numbered it), so every connection, database and subscription handle from before the loss is stale; peer handles stay usable. CoreBluetooth and BlueZ then emit `backend-restarted`. Each OS keeps the sequence its 4.x backend had. The manager survives the loss and binds the new generation, so a connection supervisor reconnects when the adapter returns and a release of anything the loss ended answers `released` (4.x destroyed the manager on a loss).

The core publishes lifecycle, scan-end, security, write-readiness and adapter-reset events on bounded queues (256 each). A backend that falls behind is told how many it missed, and re-reads the core's own state rather than guessing: every live link's connection state and generation, and its database state (`connection-lost`, `database-changed`, or the adapter-loss sequence above while the adapter is lost); which scan the core still owns (a scan it no longer owns ends `source-failed`); and each watched peer's security state and write readiness. A link the core still reports live and current gets no event.

A subscription's `delivery.overflowPolicy` governs the core's buffer for that consumer too, and values the radio lost before the core could hold them count against it. With `error`, such a loss ends the stream with an `overflow` terminal carrying the counts, as a local overflow does. With `drop-oldest`, `drop-newest` or `latest`, the stream reports an overflow notice with the cumulative counts and keeps delivering.

A notification value the binding hands over malformed ends that subscription's stream `source-failed` with `protocol.malformed`; it is never dropped.

Delivery is event-driven, as the 4.x native callbacks were. The addon wakes the provider as soon as it queues an advertisement, a notification value, or a lifecycle, adapter, reset, security, write-readiness or scan-end report. The provider's polls (5 ms for scans and notifications, 10 ms for the other core events) remain only as a safety net. When a link is lost, the database changes or the adapter is lost, every value the core still holds for a subscription is delivered in order before that stream's terminal.

A core error that carries the OS's own answer reports it as the 4.x platform identity: CoreBluetooth `{ domain: 'corebluetooth', code: <NSError code> }`, WinRT `{ domain: 'winrt', code, metadata: { hresult, gattStatus } }`, BlueZ `{ domain: 'bluez-dbus', code: <D-Bus error name> }`, with the OS message as `safeMessage`. An error with no OS answer carries `{ domain: 'desktop-rust-core', code: 'core-detail' }`.

### Option audit

Each option below is rejected before any core call. A test per row asserts that no dispatch reaches the core (`__tests__/backends/desktop/desktop-factories.test.js`).

| Option                                                   | Behaviour                                                                                                                                                                                                                                                                                                                                                                                                                           |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scan query / filters                                     | Required service UUIDs go to the OS scan filter (`scanner.plan`). A caller's `localNamePrefix` also goes to the OS: BlueZ receives it as the `SetDiscoveryFilter` `Pattern`, as the 4.x dbus-next backend sent it; CoreBluetooth and WinRT have no OS name filter. `Pattern` also matches an address prefix, so the OS only narrows. Name-prefix, manufacturer and address predicates always match in software as the final filter. |
| Scan `duplicatePolicy`                                   | Carried to the OS scan: `all` asks for every advertisement; `first` and `merged` ask the OS to filter repeats (BlueZ `DuplicateData: false`, CoreBluetooth `AllowDuplicates: NO`). `first` also delivers one sighting per peer per consumer. `merged` is the default of `scanForServices` / `scanUntil`.                                                                                                                            |
| Scan `platform` options                                  | `capability.unsupported` (not registered)                                                                                                                                                                                                                                                                                                                                                                                           |
| Scan share / join                                        | One core scan fanned out to joined leases. A forged token is `ownership.denied`.                                                                                                                                                                                                                                                                                                                                                    |
| `deliveryMode` `require-*`                               | Checked against the characteristic's properties before any dispatch (`gatt.property-not-supported`). The delivery a value reports is what the core observed.                                                                                                                                                                                                                                                                        |
| Write without response                                   | Resolves `commitState: 'unknown'` (never `confirmed`)                                                                                                                                                                                                                                                                                                                                                                               |
| Descriptor write without response                        | `capability.unsupported`, as the legacy WinRT addon answered: descriptors are written with a response                                                                                                                                                                                                                                                                                                                               |
| Connection `intent: 'when-available'`, `preferredPhy`    | `capability.unsupported`                                                                                                                                                                                                                                                                                                                                                                                                            |
| Connection `transport` other than `le`/`auto`            | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `bluezBus` / `pairingGeneration` on a non-BlueZ provider | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Unknown `busKind` (BlueZ)                                | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `restoration`                                            | refused by the Node host                                                                                                                                                                                                                                                                                                                                                                                                            |

Cancellation: an aborted `AbortSignal` cancels exactly the in-flight core operation, through a ticket the host mints before the call. An abort before admission ends the operation with `operation.aborted` and no radio call. A caller deadline crosses to the core as a relative budget. Without one, the core's named liveness backstops apply.

### Parity with the 4.x desktop backends

Every capability the TypeScript CoreBluetooth, WinRT and dbus-next BlueZ backends offered is tracked in `DESKTOP_RUST_CORE_PARITY`, which `unified-ble-manager/testing` exports (no production entrypoint exports it). A capability is registered only when the loaded core implements it on this OS (`UbmCentral.capabilityStates`) and this provider wires it.

**Implemented on the Rust path:**

- Scanning: the OS service-UUID scan filter; software name, manufacturer and address filters; scan share/join; first-sighting duplicates.
- Events: `connection-lost` and `database-changed`.
- Adapter: power, authorization (macOS, Windows), watch, and enumeration/selection.
- Operations: exact in-flight cancellation; an honest without-response commit state; the `require-*` delivery check. WinRT additionally carries the requirement to the OS and prefers notify over indicate.
- Connection: CoreBluetooth connected RSSI; maximum write length and long write; Windows maintain-connection.
- Advertisements: solicited and overflow UUIDs and `connectable` (macOS).
- Security (Windows, Linux): state, pair, cancel, unpair and security events, plus the BlueZ pairing-generation controller.
- Linux: address targeting, advertisement address type, extended characteristic flags and access requirements, and the priority/parameter reasons.

- macOS: the write-without-response readiness watch (`DesktopCentral::write_readiness`). It is registered only where the core reports a readiness signal.
- Windows: the scan-terminated event, from the core's scan source-closed terminal.
- Linux: adapter listing and selection on the session bus (`list_adapters_on`).

- LEGACY-AUDIT-1:
  - the adapter-loss teardown and generation advance;
  - admission errors;
  - the CoreBluetooth first-state wait;
  - `resetting` / `unsupported` states;
  - `merged` scans that reach the OS duplicate filter, and LE-only BlueZ scans;
  - repeated service, characteristic and descriptor UUIDs that keep their instances;
  - uncached WinRT discovery;
  - maximum write length without discovery;
  - WinRT selection of any listed adapter, with its `deployment`;
  - the 4.x backend and adapter ids;
  - the `unsupported` rows that keep their 4.x reasons.

  The rows with kept reasons are CoreBluetooth `connection:request-mtu` (`corebluetooth-auto-negotiated-mtu`), `connection:effective-mtu` and `connection:phy`, and BlueZ `security:pairing-generation`. The BlueZ row carries the privilege explanation without a host controller, and the adapter-wide blast radius with one. The vendored btleplug patches the loaded core links are reported as `diagnostics.btleplugPatches`.

**Open release blockers:** none on the TypeScript/N-API path. `__tests__/backends/desktop/desktop-parity-blockers.test.js` fails if a row is ever marked `blocked` without a probe. The deterministic synthetic radio does not prove physical-radio behaviour. The physical checks listed under [Verification](#verification) are still outstanding.

## Advanced provider construction

> **Maintainer/host-authoring reference — not ordinary application construction.**
> Use the one-call factories above for application code. This provider example
> is for maintainers implementing a host integration or authors wiring an
> explicitly selected backend; it is not the normal application recipe.

```ts
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } from 'unified-ble-manager/advanced'
import {
  coreBluetoothCompatibility,
  createNativeCoreBluetoothBackendProvider
} from 'unified-ble-manager/node/corebluetooth'

const now = () => performance.now()
const provider = createNativeCoreBluetoothBackendProvider({ now })
const adapters = await provider.listAdapters()
if (adapters[0] === undefined) {
  throw new Error('No CoreBluetooth adapter is available.')
}

const manager = await createBleManagerFromProvider(
  {
    provider,
    selection: { selectedAdapterId: adapters[0].adapterId },
    coreCompatibility: coreBluetoothCompatibility,
    manager: {
      clientId: 'node-corebluetooth-client',
      managerId: 'node-corebluetooth-manager',
      ownerMode: 'owning'
    }
  },
  { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
)
```

WinRT has the same shape, with `createNativeWinRtBackendProvider` and `winRtCompatibility` from `unified-ble-manager/node/winrt`. BlueZ:

```ts
import { createDbusNextBluezBackendProvider } from 'unified-ble-manager/node/bluez'

const provider = createDbusNextBluezBackendProvider({ busKind: 'system', now })
```

The name `createDbusNextBluezBackendProvider` is historical: it returns the shared Rust core provider, and no dbus-next transport is involved. All three are `createDesktopRustCoreBackendProvider({ platform, owner, now })` underneath, which every desktop entrypoint also exports. Deterministic suites use `createTestDesktopRustCoreBackendProvider` from `unified-ble-manager/testing` instead, which additionally accepts the synthetic radio and the binding, platform and first-state-timeout seams.

Then scan and GATT through the same `BleManager` as React Native. Await `manager.destroy()` when the process session ends. Its cleanup record reports every release failure the core recorded.

## Verification

Hardware-free (any host): `pnpm test:package` drives the provider through the real addon on its deterministic synthetic radio. Every verb executes in Rust. The first-party TCK legs that `unified-ble-manager/testing` exports (`createCoreBluetoothFirstPartyTckRegistration`, `createBluezFirstPartyTckRegistration`, `createWinRtFirstPartyTckRegistration`) run the same way: the `/testing` provider (`createTestDesktopRustCoreBackendProvider`) over the addon's synthetic radio, never a production open. They are deterministic proof only.

Clean packed consumer, run from the checkout:

```sh
pnpm native-prebuild:build -- --backend desktop-core   # this host's release prebuild + sidecar
pnpm prepack && npm pack --pack-destination /tmp/ubm-pack
node scripts/ci/napi-clean-tarball-acceptance.js --tarball /tmp/ubm-pack/unified-ble-manager-*.tgz --pm npm --negative
node scripts/ci/napi-clean-tarball-acceptance.js --tarball /tmp/ubm-pack/unified-ble-manager-*.tgz --pm pnpm --negative
```

The default `--probe identity` stops before any radio open. It loads the prebuild from the installed package, verifies identity, and drives a synthetic scan/connect/discover/subscribe/notify round trip in Rust. It is safe in a process without Bluetooth permission. `--probe radio` calls this OS's public no-options factory on the real adapter. Run it from a Bluetooth-authorized terminal on macOS: a headless host passes with `adapter.unavailable`.

Physical checks still outstanding: a peripheral session per OS (scan, connect, discover, notify, write, disconnect); pairing on Windows and Linux, including the BlueZ pairing-generation controller; the Windows scan-terminated event when the radio is turned off; and a session-bus BlueZ under `dbus-run-session`.

## Electron

Do not load a Node radio factory from a renderer. See [`ELECTRON.md`](ELECTRON.md).

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
