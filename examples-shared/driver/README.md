<!-- examples-shared/driver/README.md -->

# Cross-host BLE test driver

The same scenarios, driven the same way, on every host: Expo (Android and iOS),
Web Bluetooth, Tauri, Electron and a Node desktop CLI. Each host runs the shared
scenario code in this folder against the public `unified-ble-manager` API. The
host only adds a thin adapter. One control server drives all connected hosts at
once and compares what each one did.

Running a scenario is not release evidence. A support claim still needs the
retained, checksum-bound records described in [`evidence/v1/`](../../evidence/).

## Layout

| Path | What it is |
| --- | --- |
| `protocol.ts` | Wire contract `ubm-test-driver/1`, loaded by every host and by the server |
| `scenario-core.ts` | `ScenarioController`, `ScenarioRegistry`, typed command arguments, console runtime |
| `scenarios/*.ts` | `h10-stream`, `link-loss`, `device-info`, `mtu`, `scan-details`, `ecg`, `background` |
| `polar-pmd.ts` | Polar PMD (ECG) framing, from Polar's BLE SDK |
| `host.ts` | The host-adapter seam (`DriverHost`), peer acquisition, adapter readiness, capability lease |
| `user-gesture.ts` | The explicit pending-user-gesture gate (Web Bluetooth chooser) |
| `remote-channel.ts`, `create-driver.ts`, `driver-url.ts` | Host → server channel, registry factory, URL rules |
| `browser/` | Shared by the Web, Tauri and Electron renderer hosts: WebSocket, visibility app state, the scenario panel |
| `server/` | Control server and CLI (`cli.mjs`), hub, sequences |

A host adapter supplies only the manager factory and readiness step, the
platform label, the WebSocket, the app-state source and driver-URL discovery:

| Host | Adapter | Manager | App state | WebSocket | Driver URL |
| --- | --- | --- | --- | --- | --- |
| Expo | `example-expo/src/driver/app-driver.ts` | `createExpoBleManager` + readiness/permission | React Native `AppState` | RN `WebSocket` | Metro bundle host, or `EXPO_PUBLIC_UBM_DRIVER_URL` |
| Web | `example-web/src/driver.ts` | `createWebBleManager` | page visibility | browser | page host, or `?driver=` |
| Tauri | `example-tauri/src/driver.ts` | `createTauriBleManager` (Tauri IPC) | page visibility | browser | local server, or `?driver=` |
| Electron | `example-electron/driver/` | main: desktop provider + router/binding; renderer: `createElectronRendererBleManager` | page visibility | browser | local server, or `--driver-url` |
| Node | `example-node/host.ts` | `unified-ble-manager/node/{corebluetooth,winrt,bluez}` | none (`untracked`) | Node `WebSocket` | local server, or `--driver-url` / `UBM_DRIVER_URL` |

## Protocol `ubm-test-driver/1`

JSON text frames only. A host connects to `ws://<server>:8795/host` and sends
`hello`: `protocol`, `host` (`expo` | `web` | `tauri` | `electron` | `node`),
`platform` (`android` | `ios` | `macos` | `windows` | `linux` | `unknown`),
`backend` (the stack the adapter built, for example `node/corebluetooth`),
`model`, `osVersion`, `appBuild` and `scenarios`. The server answers with a
`welcome` carrying the `hostId` (`<host>-<platform>-<model>`). Events and
snapshots carry `host: "<host>/<platform>"`.

The protocol fails closed. A hello from another protocol version (including the
retired `ubm-phone-driver/1`), an unknown host kind, or a hello missing
`backend` gets close code 4400 and a `host-rejected` record. A WebSocket upgrade
on any other path (including the retired `/phone`) is refused and recorded as
`upgrade-refused`. Sequence files that still use the retired `platforms` step
filter are refused before anything runs; use `hosts`.

## How a scenario acquires the Polar H10

`peerAcquisition()` reads the backend's own capability report. It never looks
at the host name or at `discovery.kind`.

- If `discovery:continuous-scan` is supported, the scenario calls `find()`.
- Otherwise, if `discovery:system-chooser` is supported, it calls `choose()`.
  The call includes `optionalServices` for Battery, Device Information and Polar
  PMD.
- Otherwise it calls `find()`, so the library answers with its own error.

Where the host has a gesture gate (Web), a chooser run first enters the phase
`awaiting-user-gesture`. It emits `user-gesture-required` and the page shows an
**Open chooser for &lt;scenario&gt;** button. Only a real click continues the run
(`user-gesture-received`). A `stop`, or 120 s without a click
(`host.user-gesture-timeout`), aborts the run. Nothing synthesizes the gesture.

Each run emits `peer-acquisition {via}`. When a host cannot do something, the
failure is the library's own answer: a typed error such as
`capability.unsupported` from the call itself, or the capability descriptor for
the background lease. It is never a skip.

## Launching the control server

From the repository root (Node ≥ 22.18):

```sh
node examples-shared/driver/server/cli.mjs serve            # listens on 0.0.0.0:8795, JSON lines + log file
node examples-shared/driver/server/cli.mjs hosts
node examples-shared/driver/server/cli.mjs describe all
node examples-shared/driver/server/cli.mjs run all h10-stream start '{"autoReconnect":true}'
node examples-shared/driver/server/cli.mjs run macos mtu probe
node examples-shared/driver/server/cli.mjs run expo-android-google-pixel-9 ecg stop
node examples-shared/driver/server/cli.mjs sequence examples-shared/driver/server/sequences/h10-stream.json --out /tmp/h10.json
```

`<target>` is `all`, a host id from `hosts`, a host kind
(`expo` | `web` | `tauri` | `electron` | `node`) or a platform
(`android` | `ios` | `macos` | `windows` | `linux`). A sequence runs on every
targeted host in parallel. It ends with a per-step comparison table that marks
the steps whose outcome differs between hosts. `npm run driver -- <command>` in
`example-expo` runs the same CLI.

## Launching each host

Every host below except Expo runs from the repository root after `pnpm prepack`,
because it imports the checkout's own built package.

### Expo (Android, iOS)

```sh
pnpm --dir example-expo install --no-frozen-lockfile
pnpm --dir example-expo exec expo prebuild --clean --no-install     # native projects, once
pnpm --dir example-expo android                                     # or: pnpm --dir example-expo ios
adb reverse tcp:8795 tcp:8795                                       # Android over USB with Metro on localhost
```

Development builds connect to `ws://<Metro host>:8795/host` on launch. The badge
on the **Test scenarios** screens shows the connection. Set
`EXPO_PUBLIC_UBM_DRIVER_URL=ws://<mac>:8795/host` (or `off`) to override. Metro
resolves the shared folder through `example-expo/metro.config.js`.

### Web (Chrome / Chromium)

```sh
pnpm prepack
pnpm exec vite --config example-web/vite.config.mts
open http://127.0.0.1:5173/driver.html                  # Web Bluetooth needs a secure context: localhost or https
```

The page connects to port 8795 on the host that served it. Use
`?driver=ws://<mac>:8795/host` to override. For Chrome on an Android phone, run
`adb reverse tcp:5173 tcp:5173` and `adb reverse tcp:8795 tcp:8795`, then open
`http://localhost:5173/driver.html` on the phone. Every run that needs a peer
waits for a click on **Open chooser for …** in the page.

### Tauri

```sh
pnpm prepack
pnpm exec vite --config example-tauri/vite.config.mts             # terminal 1: webview frontend on 127.0.0.1:1420
cargo run --manifest-path example-tauri/src-tauri/Cargo.toml      # terminal 2: a debug build loads build.devUrl
```

Click **Open the shared test driver** in the window. The page connects to
`ws://127.0.0.1:8795/host`; `?driver=` overrides it. The Rust plugin owns the
radio. On macOS, the process that launches the binary (your terminal) needs
Bluetooth permission.

### Electron

```sh
pnpm prepack
pnpm exec vite build --config example-electron/driver/vite.config.mts
pnpm exec electron example-electron/driver/main.cjs                # --backend corebluetooth|winrt|bluez, --driver-url ws://…/host
```

Main selects the backend explicitly (`--backend`, else `UBM_ELECTRON_BACKEND`,
else the OS default) and owns the radio. The sandboxed renderer uses only the
preload transport on `unified-ble-manager:v2`. On quit, main destroys the
binding, then the manager, and logs each cleanup record.

### Node desktop CLI

```sh
pnpm prepack
node example-node/driver.ts serve-host                            # --backend corebluetooth|winrt|bluez, --driver-url ws://…/host
node example-node/driver.ts run h10-stream start --for 30000      # local, no server: events as JSON lines, then stop
node example-node/driver.ts list
```

The desktop core loads from `native/desktop-core/prebuilds/<platform>-<arch>/`.
From a checkout without that prebuild, run `node scripts/ci/build-napi-addon.js`
and set `UBM_NAPI_ADDON` (see [`docs/NODE.md`](../../docs/NODE.md)). On macOS
the terminal needs Bluetooth permission.

## What each host can run

The library answers each call itself. The rows below are what the source says
to expect. They are not hardware evidence.

| Scenario | Expo Android | Expo iOS | Web | Tauri / Electron / Node (desktop core) |
| --- | --- | --- | --- | --- |
| `h10-stream`, `device-info` | runs | runs | runs after the chooser click | runs |
| `link-loss` | runs (reconnect by peer reference) | runs | runs after the chooser click | runs |
| `h10-stream` / `link-loss` with `intent: "when-available"` | runs | `capability.unsupported` from `connect` | answered by `connect` | answered by `connect` |
| `mtu` | runs | `requestMtu`, `effectiveMtu`, `readPhy` report unsupported (CoreBluetooth negotiates the MTU); write length measured | each probe reports the backend's answer | each probe reports the backend's answer (macOS: CoreBluetooth rows unsupported) |
| `scan-details` | runs | runs | `scan()` refuses: Web Bluetooth has no continuous scan (`web:continuous-scan` unsupported) | runs |
| `ecg` | runs | reads the PMD control point while it is notifying, which exercises the library's read-while-notifying path | runs after the chooser click (PMD is in `optionalServices`) | runs |
| `background` | Expo lease API; app state from `AppState` | same | lease: `web:background-operation` descriptor (unsupported); app state from page visibility | lease: `background:desktop-maintain-connection` descriptor (registered on WinRT only); Tauri/Electron use page visibility; Node reports `untracked` (a CLI has no app lifecycle), so `sequences/background.json` cannot pass there |

## Tests and type checks

```sh
node --disable-warning=MODULE_TYPELESS_PACKAGE_JSON --test \
  'examples-shared/driver/__tests__/*.test.mjs' 'examples-shared/driver/server/__tests__/*.test.mjs' \
  'example-node/__tests__/*.test.mjs' 'example-electron/driver/__tests__/*.test.mjs' 'example-tauri/__tests__/*.test.mjs'
pnpm --dir example-expo test:driver                    # shared + server + Expo adapter tests

pnpm exec tsc -p examples-shared/driver/tsconfig.json
pnpm exec tsc -p example-node/tsconfig.json
pnpm exec tsc --noEmit -p example-web/tsconfig.json
pnpm exec tsc -p example-tauri/tsconfig.json
pnpm exec tsc -p example-electron/driver/tsconfig.json
pnpm --dir example-expo exec tsc --noEmit
```

The shared tests cover the protocol, the registry and scenario core, the remote
channel, Polar PMD parsing, the scenarios against a recording manager double,
the user-gesture gate, the browser host facts and the server. The server tests
cover the hub, the CLI client, sequences and the WebSocket. The scenario tests
pin which call each scenario makes: scan versus chooser, the gesture gate, the
ECG order, and the background lease and app state. Each host also has an
adapter test:

- Expo: Metro resolution;
- Node: backend selection, identity and hub registration;
- Electron: the preload surface;
- Tauri: single-instance Vite resolution.
