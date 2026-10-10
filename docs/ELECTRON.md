<!-- docs/ELECTRON.md -->

# Electron

Main owns the radio. The renderer uses a versioned IPC client and never loads a native addon.

This source targets `5.0.2`. Main executes the shared Rust core (`DesktopCentral`) through one N-API addon. Tagged releases ship it prebuilt for macOS Apple Silicon (`arm64`) and Windows/Linux `arm64`/`x64`. The addon is Node-API, so one binary serves Node and modern Electron alike.

macOS desktop support is Apple Silicon (`arm64`) only. Windows and Linux desktop support includes `arm64` and `x64`.
Intel macOS desktop is outside the UBM support policy, including source-built
Electron desktop consumers. This is UBM's package policy, not a blanket claim
about Apple's macOS support lifecycle.

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

The renderer's shared event stream also has a bounded buffer. If it overflows,
the public manager ends every active child stream because the lost events cannot
be assigned reliably to individual scans or subscriptions. The resulting error
reports aggregate loss with `attribution: 'unknown'`; it does not claim exact
per-stream counts. A failed event acknowledgment or event iterator likewise
ends active streams with its failure cause. The application must call
`manager.destroy()` after such a terminal and retry a failed cleanup receipt;
the renderer retains its remote release ownership until cleanup succeeds.

### Connection and GATT recovery

Eligible Linux `acquireWrite()` and `acquireNotifications()` calls cross the
trusted-main route to the same native central used by Node. Renderers receive
opaque handles and copied packets. The [acquired transport contract](NODE.md#explicit-bluez-acquired-gatt-transports)
applies to MTU limits, backpressure, cancellation and commit uncertainty.
Iterator return closes an acquired notification transport. Main retains failed
close ownership through rediscovery, connection release, renderer reload and
admission rollback; another renderer lease cannot use or close the handle.

The shared IPC client used by Electron and Tauri invalidates a GATT generation
and publishes its change cause before waiting for subscription cleanup. Its
handles become unusable immediately; an unsubscribe failure cannot restore
validity. Subscriptions admitted during early-event replay remain owned until
their release is confirmed.

Releasing one connection closes new-work admission immediately, observes a
bounded child-cleanup drain, then requests that connection's existing scoped
parent release even if a child rejects or remains pending. This retains lease
ownership semantics; it is not an unconditional physical disconnect that drops
another client's shared connection. Confirmed parent release retires that
generation's native obligations. Refused parent release leaves them owned and
retryable. Neither outcome proves that arbitrary local iterator cleanup
succeeded: local failures remain separately reportable and retryable.

Discovery's wait for old database cleanup observes the caller's original
deadline and abort signal without cancelling or forgetting the cleanup itself.
Buffered stream replay stops after its destination terminates or is replaced,
so one winning terminal schedules one owner-cleanup attempt while preserving
known upstream loss counters. An explicit later cleanup retry remains possible.

There is no Noble dependency, renderer Web Bluetooth fallback, legacy
`BlePort`, `PortBleManager`, or mock-radio production fallback in these
entrypoints.

The release workflow's packed Electron smoke is deterministic L1 package/IPC
proof, not an Electron host, adapter, or peripheral support claim. Native
prebuild compilation and runtime loading are L2/L3 evidence only; they do not
by themselves establish a physical-radio support claim.

## Native continuation owned by main

`unified-ble-manager/electron/main` exports `loadDesktopCoreBinding` and
`createNativeContinuationController`, with the same trusted-host recipe as
[Node native continuation](NODE.md#native-continuation-in-a-trusted-process-host).
Pass the **existing main-process Rust central** to the controller. Main retains
the native owner while windows close or reload; renderer destruction is not a
request to stop a process-owned recorder.

```ts
import { createNativeContinuationController } from 'unified-ble-manager/electron/main'
import type { DesktopRustCoreCentral } from 'unified-ble-manager/electron/main'

function recorderForMain(central: DesktopRustCoreCentral) {
  return createNativeContinuationController(central)
}
```

Only trusted main code may call `execute`, `status` and `claim`. This adds no
renderer IPC privilege: expose any application-specific commands through your
existing authenticated, scoped IPC policy. `execute` needs a known peer ID and
explicit native resubscription declaration. `claim` validates values before
acknowledging native handoff, reports loss and cleanup uncertainty, and ends the
recording generation. Persist returned values according to your application.

For an authenticated application bridge that forwards raw native control
envelopes, main may use `encodeNativeContinuationFailure(error)` when a typed
native-compatible failure was already decoded before dispatch (for example,
recording-store configuration). This preserves the error code, operation,
retryability and platform message/metadata across Electron's string-only error
rejection transport. Keep authentication and request validation outside this
conversion. Unknown exceptions are rethrown. This is not general `BleError`
serialization: write commit states, limitations and nested/binary metadata are
not representable in the native control envelope and are explicitly refused.

This survives loss of a renderer, **not loss of the main process**. A bounded
in-memory backlog is not disk persistence, and Electron relaunch/start-at-login
configuration is the application's responsibility. On main shutdown, await the
central's cleanup receipt; a failed receipt remains a retry obligation. Do not
start a second central to add continuation to an existing host.

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

Only the shared Rust desktop core is produced for Node/Electron distribution.
The unreachable C++ CoreBluetooth/WinRT addons and their private loaders have
been removed. The canonical matrix retains macOS arm64 and Windows/Linux x64
and arm64; native identity, hash and packed-content checks still apply. Source
builds use the same Rust producer, not a parallel node-gyp implementation.

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

Security authorization also comes from trusted main-process facts. The sender
returned by `authenticate(event)` may include `securityPermissions`:
`security:state` authorizes state and watch, `security:pair` authorizes pairing,
`security:cancel-pairing` authorizes cancellation, `security:unpair` authorizes
bond removal, and `security:custom-ceremony` authorizes custom responses. An
omitted list defaults to no security permissions. Grant only the operations
your policy allows for that authenticated window/session; never copy grants
from a renderer request. The grant remains fixed for the attachment. The
reference driver grants the first four explicitly and leaves custom ceremony
denied. Permissions authorize a route; the attached native backend still
determines capability and reports the operation's actual result.

The renderer and main negotiate the IPC protocol at bootstrap; both offer
exactly version 6. Version 5 added security routes, address targeting, connection
intent and platform scan-option forwarding. A capability still describes the
instantiated backend's implementation; transport support does not invent native
support. The caller's deadline crosses as a relative `budgetMs` that
main admits against its own monotonic clock at receipt (the renderer's
`performance.now()` instant has a different time origin), and main rejects an
absolute renderer `deadline` as `protocol.malformed`. Normalized errors may
carry `commit` (`not-dispatched`, `uncertain`, or `null`). Version 4 adds the
attachment rebind described below. A renderer and main built from different
package versions where one side speaks protocol 4 or older fail at bootstrap
with `protocol.incompatible`, in either direction, before a lease is
registered or any operation runs, so preload, renderer bundle and main must
ship from the same package version. The IPC channel name
(`unified-ble-manager:v2`) is unchanged so the refusal arrives as a typed error
rather than a missing handler.

**Adapter loss and the attachment rebind (introduced in protocol 4).** An adapter loss
moves the main-process manager to the backend's new attachment (new backend
and adapter generations); the manager itself stays alive. Main, never a
renderer, then rebinds every active renderer lease to that attachment and
announces it on the reserved `attachment` stream as one event whose item is
`{ kind: 'value', value: { kind: 'backend-restarted', schemaVersion: 1,
previousAttachmentId, attachmentId, attachment } }`. The renderer adopts only
an announcement for its own lease that names the attachment it holds, on the
same backend instance; anything else is refused and reported, and a renderer
can never choose an attachment. Until the announcement arrives, and for the
replaced attachment afterwards, main refuses work with `backend.reset` before
any radio effect, except the releases a renderer still owes (`operation.cancel`,
`scan.stop`, `gatt.unsubscribe`, `gatt.database.release`,
`connection.disconnect`, `connection.events.unsubscribe`). Links, scans and
subscriptions from before the loss have ended; a connection supervisor waits
for the adapter and reconnects through the same renderer manager. Before 5.0
the loss destroyed the main-process manager and every renderer had to be
recreated.

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
`linux-arm64`, `darwin-arm64`, `win32-x64` and `win32-arm64`,
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

The shared Rust binding requires Node-API v4 (`napi4`), not Electron's
runtime-specific module ABI. A different Node/Electron module ABI alone does
not require rebuilding; OS/architecture, platform dependencies, Node-API
compatibility and the sealed UBM build identity still must match. The Electron load smoke verifies
identity and runs a synthetic central; it does not start a real scan, observe
an advertisement, or establish live-radio support. Published evidence
records state the exact backend, package digest, OS/runtime/ABI, hardware,
scenario, limitations, and proof level.
See [`PLATFORMS.md`](PLATFORMS.md) and the controlling
[Current 5.0 authority](README.md#current-50-authority).

IPC version 6 requires nullable observed service graph facts and the native
readiness/acquired-GATT route contract. Version-5 peers are refused during
bootstrap, before a renderer lease or radio effect is created.

Connection parameter/readiness streams preserve the shared
[observation ownership and source recovery rules](NODE.md#connection-observation-ownership)
through main-process IPC. Windows renderers can
[read the observed TX/RX PHY](NODE.md#windows-phy-observation) when their
instantiated backend reports that runtime capability; this grants no PHY
request or selection support.
