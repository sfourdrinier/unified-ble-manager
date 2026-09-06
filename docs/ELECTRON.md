<!-- docs/ELECTRON.md -->

# Electron

Main owns the radio. The renderer uses a versioned IPC client and never loads a native addon.

This source targets `4.0.24`. Tagged releases ship Node-API v8 prebuilds covering macOS and Windows `arm64`/`x64` for both Node and modern Electron.

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

Select one concrete backend in main. The native loaders are fail-closed:

- `createElectronMainCoreBluetoothBackendProvider({ now })` loads only the
  package-controlled CoreBluetooth Node-API artifact and rejects non-macOS
  hosts.
- `createElectronMainWinRtBackendProvider({ now })` loads only the
  package-controlled WinRT Node-API artifact, requires Windows, and verifies
  native boundary protocol v2. Its private boundary fixes scan ownership at
  `startScan(scanToken, serviceUuids, onAdvertisement)` and requires the
  `onScanTerminal(listener)` registration method; a v1 or incomplete artifact
  is rejected rather than adapted.
- `createDbusNextBluezBackendProvider({ busKind, now })` constructs the owned
  BlueZ D-Bus backend for the explicitly selected system or session bus.

An Electron application chooses the backend from trusted main-process platform
configuration. Renderer-provided data is never a backend selector. Native
addons must be available for the host platform and architecture; an absent,
incompatible, unauthorized, or unavailable native backend reports a typed
failure rather than silently using a simulated radio. Published builds use
Node-API v8, so one prebuild per OS/CPU is shared by maintained Node and modern
Electron runtimes.

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

Published packages include these Node-API prebuilds:

- macOS `arm64` and `x64` CoreBluetooth;
- Windows `arm64` and `x64` WinRT.

The release matrix builds each artifact on its native runner and loads the same
file under both Node and Electron before npm publication. Native `.node` files
must remain unpacked from ASAR and must be included in the consumer
application's signing/notarization process.

Source and `node-gyp` remain available as an explicit fallback. For example:

```sh
pnpm --dir node_modules/unified-ble-manager exec node-gyp rebuild --release --directory native/electron/corebluetooth
```

Published prebuilds remain the default after a source build. To deliberately
load the local `build/Release` (then `build/Debug`) artifact before the bundled
prebuild, set the explicit process-level override:

```sh
UNIFIED_BLE_MANAGER_NATIVE_SOURCE=1 your-electron-command
```

With Node-API v8, an Electron-targeted rebuild is not required merely because
Electron's module ABI differs from Node's. Rebuild only when using an
unsupported target, changing native source, or deliberately overriding the
bundled prebuild. Windows and Linux still have their own native/runtime
requirements and are not implied by a macOS build. The Node/Electron load smoke
checks the method surface and destroys the boundary; it does not start a scan,
observe an advertisement, or establish live-radio support. Published evidence
records state the exact backend, package digest, OS/runtime/ABI, hardware,
scenario, limitations, and proof level.
See [`PLATFORMS.md`](PLATFORMS.md) and the controlling
[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md).
