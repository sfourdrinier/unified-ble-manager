<!-- docs/ELECTRON.md -->

# Electron

Main owns the radio. The renderer uses a versioned IPC client and never loads a native addon.

This source targets `5.0.0-rc.0`. Main executes the shared Rust core (`DesktopCentral`) through one N-API addon. Tagged releases ship it prebuilt for Linux, macOS and Windows on `arm64`/`x64`. The addon is Node-API, so one binary serves Node and modern Electron alike.

`unified-ble-manager/electron/main` and
`unified-ble-manager/electron/renderer` are the only Electron entrypoints.

Runnable composition lives in [`example-electron/`](../example-electron/)
(`composition-main.js`, `composition-preload.js`, `composition-renderer.js`). Sequence:

1. create a main-process provider and `BleManager`;
2. create the router and install the binding;
3. authenticate `WebContents`;
4. expose a narrow preload bridge (no generic `ipcRenderer`);
5. create the public `BleManager` with `createElectronRendererBleManager({ transport })`;
6. use the same `scan`/`find`, `connect`, `discover`, `read`, `subscribe`, and `destroy` vocabulary as other hosts;
7. release renderer resources, destroy the binding, destroy the manager.

BrowserWindow must use `contextIsolation: true` and `nodeIntegration: false`.
Security internals live in [`ELECTRON_SECURITY_MODEL.md`](ELECTRON_SECURITY_MODEL.md).

In the renderer, after preload hands you an authenticated transport:

```ts
import { createElectronRendererBleManager } from 'unified-ble-manager/electron/renderer'

const abort = new AbortController()
const manager = await createElectronRendererBleManager({ transport })
const scan = await manager.scan({
  query: { anyOf: [{ services: { any: ['180d'] } }] },
  signal: abort.signal,
  timeoutMs: 15_000
})
await scan.stop()
await manager.destroy()
```

The low-level `ElectronRendererBleClient` is an implementation seam for the
transport and tests, not an application API.

## Advanced main-process provider construction

> **Maintainer/host-authoring reference — not ordinary application construction.**
> The renderer application uses `createElectronRendererBleManager({ transport })`.
> The provider construction below is only for maintainers implementing the
> trusted main-process host boundary or authors wiring an explicit backend.

```ts
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } from 'unified-ble-manager/advanced'
import {
  coreBluetoothCompatibility,
  createElectronMainCoreBluetoothBackendProvider
} from 'unified-ble-manager/electron/main'

const now = () => performance.now()
const provider = createElectronMainCoreBluetoothBackendProvider({ now })
const adapters = await provider.listAdapters()
if (adapters[0] === undefined) {
  throw new Error('No adapter is available.')
}
const manager = await createBleManagerFromProvider(
  {
    provider,
    selection: { selectedAdapterId: adapters[0].adapterId },
    coreCompatibility: coreBluetoothCompatibility,
    manager: {
      clientId: 'electron-main-client',
      managerId: 'electron-main-manager',
      ownerMode: 'owning'
    }
  },
  { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
)
```

Main and renderer stay split:

- the Electron **main** process creates one selected owned backend and owns the
  generic manager/radio lifecycle;
- the preload exposes a narrow versioned IPC transport to the renderer;
- the renderer uses the public `BleManager` and can never select a radio,
  access a native addon, or impersonate another renderer;
- `ElectronMainBleBinding` authenticates each `WebContents` from host facts,
  owns the attachment/session mapping, bounds outbound events, and cleans up
  on navigation, renderer destruction, app shutdown, and backend restart.

There is no Noble dependency, renderer Web Bluetooth fallback, legacy
`BlePort`, `PortBleManager`, or mock-radio production fallback in these
entrypoints.

The release workflow's packed Electron smoke is deterministic L1 package/IPC
proof, not an Electron host, adapter, or peripheral support claim. Native
prebuild compilation and runtime loading are L2/L3 evidence only; they do not
by themselves establish a physical-radio support claim.

## Main-process backend selection (maintainer/host-authoring reference)

> **Maintainer/host-authoring reference — not ordinary application construction.**
> Backend selection belongs to trusted Electron main-process host code; renderer
> application code must not construct providers or select radios.

Select one concrete backend in main. Every provider is the shared Rust core
for one OS, and each refuses the wrong OS before anything loads:

- `createElectronMainCoreBluetoothBackendProvider({ now })` for macOS.
- `createElectronMainWinRtBackendProvider({ now })` for Windows (adapters
  listed and selectable by id).
- `createElectronMainBluezBackendProvider({ now, busKind?, pairingGeneration? })`
  for Linux. It takes the Node BlueZ factory's options: `busKind` (`'system'`
  by default, or `'session'`) and a host-supplied privileged
  `pairingGeneration` controller. See
  [`NODE.md`](NODE.md#bluez-bus-and-pairing-generation) for what each does and
  the privilege a controller carries.

An Electron application chooses the backend from trusted main-process platform
configuration. Renderer-provided data is never a backend selector. The addon
is loaded from the package's own `native/desktop-core/prebuilds/<platform>-<arch>/`
and its build identity is checked before any radio call. An absent,
mismatched, unauthorized or unavailable core reports a typed failure (the
table in [`NODE.md`](NODE.md#load-and-identity-failures)), never a simulated
radio.

**Bundlers:** keep `unified-ble-manager/native/desktop-core` external and
unpacked from ASAR (for example `asarUnpack: ['**/native/desktop-core/**']`,
and mark it external in webpack/vite/esbuild). The loader finds the addon
relative to its own file, so inlining it into a bundle, or moving the `.node`
away from `index.js`, turns into `no-prebuilt-for-target`. Include the
`.node` file in your signing/notarization process.

**Deadlines across the IPC boundary:** the renderer never sends its
`performance.now()` deadline to main, because the two processes have
different clock origins. It sends `budgetMs`, the remaining budget in whole
milliseconds measured just before the request is sent (`0` when the deadline
has already passed; absent when the caller gave none). Main admits the budget
against its own clock on receipt, so time queued in main counts against it,
and the core bounds the operation with that budget. Without one, the core's
liveness backstops apply.

**Legacy node-gyp boundaries (5.0 only, Phase 4 deletion):** the TypeScript
CoreBluetooth/WinRT backends and their node-gyp addons
(`native/electron/{corebluetooth,winrt}`) remain in source until the Rust path
is verified end to end, but no public entrypoint reaches them. For the record
of what is being retired: the WinRT addon implements
native boundary protocol v2, whose private boundary fixes scan ownership at
`startScan(scanToken, serviceUuids, onAdvertisement)` and requires the
`onScanTerminal(listener)` registration method.

## IPC integration requirements

Install one `ElectronMainBleBinding` on `ipcMain` with:

- an `ElectronMainBleRouter` backed by the main-process manager;
- an `authenticate(event)` function deriving the trusted attachment, renderer,
  and client identity solely from `WebContents`/session facts;
- a preload transport that implements the structural
  `ElectronRendererIpcTransport` contract and exposes no generic IPC channel.

The IPC port must pass the full authenticated invoke-event frame identity to
the binding. The binding admits only the `WebContents.mainFrame`, releases all
leases on main-frame cross-document navigation or renderer-process exit, and
waits for that cleanup before a replacement document can bootstrap. Child
frames cannot bootstrap, route, release, or acknowledge BLE ownership.

The renderer creates the public manager from the preload transport and calls
`destroy()` during its own teardown. The public factory initializes the
low-level client internally. The main process calls `binding.destroy()` before
it destroys the manager. The binding handles operation correlation, event
acknowledgement, bounded backpressure, cancellation routing, and retryable
cleanup; applications must not duplicate those policies.

The renderer and main negotiate the IPC protocol at bootstrap; both offer
exactly version 3. Version 3 sends the caller's deadline as a relative
`budgetMs` that main admits against its own monotonic clock at receipt (the
renderer's `performance.now()` instant has a different time origin), and main
rejects an absolute renderer `deadline` as `protocol.malformed`. Normalized
errors may carry `commit` (`not-dispatched`, `uncertain`, or `null`). A
renderer and main built from different package versions where one side speaks
protocol 2 fail at bootstrap with `protocol.incompatible`, in either
direction, before a lease is registered or any operation runs, so preload,
renderer bundle and main must ship from the same package version. The IPC
channel name (`unified-ble-manager:v2`) is unchanged so the refusal arrives as a
typed error rather than a missing handler.

For a connected opaque handle, `subscribeConnectionEvents(connectionHandle)`
returns a versioned lifecycle subscription. Its `events` stream contains
`ConnectionLifecycleEvent` projections, including the exact connection
generation, plus an explicit terminal record; `unsubscribe()` detaches only
that renderer consumer and is retryable when main reports cleanup failure. The subscription
never polls, never exposes a native handle, and never closes the main-owned
connection. Lifecycle consumption is exclusive per renderer-owned connection:
a second subscription is rejected rather than competing for the single source
iterator. The renderer generates the opaque stream handle, installs its local
bounded stream, then sends the internal readiness acknowledgement; main does
not pump any lifecycle record until that acknowledgement succeeds. Renderer and
main both quarantine events whose attachment or connection generation no longer
matches the subscription.

## Verification and evidence

The packed-artifact L1 smoke proves the installed public Electron main/router,
authenticated IPC binding, and renderer client across the deterministic scan →
connect → discover → read → notify → destroy journey. It also runs a clean
consumer package-boundary fixture: it loads only the documented main and
renderer entrypoints from the installed tarball, rejects private export paths,
and checks a data-only Node VM preload-surface membrane. That membrane uses
only serialized bootstrap/release data and context-realm code with string and
WebAssembly code generation disabled; it asserts that common constructor
escapes cannot obtain `process` or `require`.

This is deliberately narrower than Electron runtime security proof. It does
not execute Electron and does not establish `contextIsolation`, preload
configuration, Electron IPC permissions, an Electron ABI, or live-radio
behavior. Applications must enable and verify their actual Electron security
settings in an Electron runtime.

```sh
pnpm prepack
node scripts/ci/pack-install-smoke.js
```

`node example-electron/smoke.js` is a local published-entrypoint
public-manager scenario only. It is useful as a fast deterministic check, but
it does not substitute for the packed router/client boundary smoke or an
Electron-runtime security test.

Published packages include the desktop-core prebuild for `linux-x64`,
`linux-arm64`, `darwin-arm64`, `darwin-x64`, `win32-x64` and `win32-arm64`,
each with an identity sidecar (`ubm_desktop_core.identity.json`: the file's
sha256 and the binary's own build identity). The release matrix builds each on
its native runner, then loads it under Electron main: the identity is
verified, and one central is opened and closed on the synthetic radio
(`scripts/ci/electron-main-smoke.js`). Native `.node` files must remain
unpacked from ASAR and must be included in the consumer application's
signing/notarization process.

Consumers never build the core. Contributors working from a checkout can load
their own build by setting `UBM_NAPI_ADDON` to its absolute path. It is used
exclusively, and its digests are still checked:

```sh
node scripts/ci/build-napi-addon.js
UBM_NAPI_ADDON="$PWD/bindings/napi/ubm_echo.$(node -p 'process.platform + "-" + process.arch').node" your-electron-command
```

With Node-API v8, an Electron-targeted rebuild is never required because
Electron's module ABI differs from Node's. The Electron load smoke verifies
identity and runs a synthetic central; it does not start a real scan, observe
an advertisement, or establish live-radio support. Published evidence
records state the exact backend, package digest, OS/runtime/ABI, hardware,
scenario, limitations, and proof level.
See [`PLATFORMS.md`](PLATFORMS.md) and the controlling
[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md).
