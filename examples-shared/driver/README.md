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
| `scenario-core.ts` | `ScenarioController`, `ScenarioRegistry` (including `stopAll`), typed command arguments, console runtime |
| `scenarios/*.ts` | `h10-stream`, `link-loss`, `device-info`, `mtu`, `scan-details`, `ecg`, `background`, `restoration`, `h10-capture`, `live-dashboard` |
| `polar-pmd.ts` | Polar PMD (ECG) framing, from Polar's BLE SDK |
| `host.ts` | The host-adapter seam (`DriverHost`), peer acquisition, adapter readiness, capability lease |
| `user-gesture.ts` | The explicit pending-user-gesture gate (Web Bluetooth chooser) |
| `remote-channel.ts`, `create-driver.ts`, `driver-url.ts` | Host → server channel, registry factory, `disposeDriver` (hot-reload teardown), URL rules |
| `browser/` | Shared by the Web, Tauri and Electron renderer hosts: WebSocket, visibility app state, the scenario panel |
| `server/` | Control server and CLI (`cli.mjs`), hub, sequences |

A host adapter supplies only the manager factory and readiness step, the
platform label, the WebSocket, the app-state source and driver-URL discovery:

| Host | Adapter | Manager | App state | WebSocket | Driver URL |
| --- | --- | --- | --- | --- | --- |
| Expo | `example-expo/src/driver/app-driver.ts` | `createExpoBleManager` + readiness/permission | React Native `AppState` | RN `WebSocket` | Metro bundle host, or `EXPO_PUBLIC_UBM_DRIVER_URL` |
| Expo (Apple TV) | same adapter (`platform: tvos`, `backend: expo/tvos`) | same                                                                                  | React Native `AppState` | RN `WebSocket`   | TV Metro bundle host, or `EXPO_PUBLIC_UBM_DRIVER_URL` |
| Expo (Android TV) | same adapter (`platform: android`, `backend: expo/android`; phone APK installed as-is, launched via `adb shell am start`) | same | React Native `AppState` | RN `WebSocket` | reversed Metro (emulator `localhost:8081` -> host `8082`), or `EXPO_PUBLIC_UBM_DRIVER_URL` |
| Web | `example-web/src/driver.ts` | `createWebBleManager` | page visibility | browser | page host, or `?driver=` |
| Tauri | `example-tauri/src/driver.ts` | `createTauriBleManager` (Tauri IPC) | page visibility | browser | local server, or `?driver=` |
| Electron | `example-electron/driver/` | main: desktop provider + router/binding; renderer: `createElectronRendererBleManager` | page visibility | browser | local server, or `--driver-url` |
| Node | `example-node/host.ts` | `unified-ble-manager/node/{corebluetooth,winrt,bluez}` | none (`untracked`) | Node `WebSocket` | local server, or `--driver-url` / `UBM_DRIVER_URL` |

## Protocol `ubm-test-driver/1`

JSON text frames only. A host connects to `ws://<server>:8795/host` and sends
`hello`: `protocol`, `host` (`expo` | `web` | `tauri` | `electron` | `node`),
`platform` (`android` | `ios` | `tvos` | `macos` | `windows` | `linux` | `unknown`),
`backend` (the stack the adapter built, for example `node/corebluetooth`),
`model`, `osVersion`, `appBuild` and `scenarios`. Every command in `scenarios`
says whether it acquires a peer and takes the `device` argument
(`acceptsDevice: boolean`); a hello whose commands omit it is refused. The server answers with a
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

Each run emits `peer-acquisition {via, device}`. When a host cannot do something, the
failure is the library's own answer: a typed error such as
`capability.unsupported` from the call itself, or the capability descriptor for
the background lease. It is never a skip.

### Choosing the strap: the `device` argument

Every peer-acquiring command (`h10-stream start`, `link-loss start`,
`device-info read`, `mtu probe`, `ecg start`, `background start`) takes an
optional `device` argument, so two hosts can each run against their own strap
at the same time:

| `device` | `find()` query (`names`) | Web chooser filter |
| --- | --- | --- |
| absent | `prefixes: ["Polar H10"]` (the first H10 found, as before) | `localNamePrefix: "Polar H10"` |
| `"Polar H10 E997042F"` (exact advertised name) | `exact: ["Polar H10 E997042F"]` | `localNamePrefix: "Polar H10 E997042F"`, then the pick must be exactly that name or the run fails with `scenario.device-mismatch` |
| `"Polar H10 E99*"` (prefix, trailing `*`) | `prefixes: ["Polar H10 E99"]` | `localNamePrefix: "Polar H10 E99"` |

Web Bluetooth filters names by prefix only, which is why an exact name is
checked after the pick. The acquired peer is reported everywhere as
`peer: {id, name, query: {match, name}}`: in the scenario snapshot (next to
`device`, the name or id), in the `found` event, and in every command result:
`h10-stream`/`link-loss`/`background` `start` add `peer`; `device-info read`
returns `{peer, reads}`, `mtu probe` `{peer, probes}` and `ecg start`
`{peer, mtu, features, settings}`. `stop` takes no arguments.

### No strap? Use the H10 simulator

[`tool/h10-sim`](../../tool/h10-sim/README.md) is a Rust BLE peripheral
that impersonates a Polar H10 (HR + Battery + Device Information + PMD ECG),
with a JSON-lines TCP control port for faults (`set-bpm`, `set-silent`,
`drop-link`, `reject-next-pmd`). Point any scenario at it with the `device`
argument, e.g. `h10-stream start '{"device":"Polar H10 SIM0001"}'`.

The sim can also join the driver itself as host kind `peripheral-sim`
(`h10-sim --driver ws://host:8795/host`), exposing its controls as the
`sim-control` scenario. Combined sequences target the DUT and the sim
together: `server/sequences/h10-sim-drop-link.json` (android streams while
peripheral-sim drops the link, then values resume) and
`server/sequences/h10-sim-ecg-fault.json` (injected `reject-next-pmd`, then
the DUT's `ecg start` reports `pmd.request-rejected`). Old servers fail
closed on the new kind — they refuse its hello under `ubm-test-driver/1`.

### Connect: one explicit retry for a transient failure

A single-shot connect (`device-info`, `mtu`, `ecg`, and `h10-stream` or
`background` without `autoReconnect`) that fails with a `BleError` whose
`retryability` is `caller-decides`, such as an Android GATT 133 while the link
is being established, is retried exactly once. Before the retry the run emits
`connect-retry {attempt: 2, maxAttempts: 2, failedAfterMs, error}` with the
first error; `connected` then carries the `attempt` that succeeded. The
decision reads `retryability` from the library's `BleError`, never from the
error code or platform detail. A `never` error, a second failure, or a run
stopped meanwhile ends the run with that error. The library itself does not
retry. Supervised runs (`link-loss`, `autoReconnect: true`) keep the
connection supervisor's own retry policy.

### Hot reload never orphans a run

`ScenarioRegistry.stopAll()` stops every scenario at once and waits for each
release. It stops all of them even when one fails, then rejects with
`scenario.stop-all-failed` carrying the whole report (every release that did
not report `released`, and every stop that threw), so a cleanup failure is
never swallowed. `disposeDriver()` stops the remote channel first (no new
command can start a run), then calls `stopAll()` and logs the report. Each
host calls it before its driver module is replaced:

- Expo: `module.hot?.dispose(...)` in `app-driver.ts` (Fast Refresh). Metro
  does not await the callback, so a failure is also written with
  `console.error`.
- Web, Tauri and the Electron renderer: `import.meta.hot?.dispose(...)` in the
  Vite entry; Vite awaits the returned promise.

## Launching the control server

From the repository root (Node ≥ 22.18):

```sh
node examples-shared/driver/server/cli.mjs serve            # listens on 0.0.0.0:8795, JSON lines + log file
node examples-shared/driver/server/cli.mjs hosts
node examples-shared/driver/server/cli.mjs describe all
node examples-shared/driver/server/cli.mjs run all h10-stream start '{"autoReconnect":true}'
node examples-shared/driver/server/cli.mjs run android h10-stream start '{"device":"Polar H10 E997042F"}'
node examples-shared/driver/server/cli.mjs run macos mtu probe
node examples-shared/driver/server/cli.mjs run expo-android-google-pixel-9 ecg stop
node examples-shared/driver/server/cli.mjs sequence examples-shared/driver/server/sequences/h10-stream.json --out /tmp/h10.json
node examples-shared/driver/server/cli.mjs sequence examples-shared/driver/server/sequences/parallel-two-straps.json
```

`<target>` is `all`, a host id from `hosts`, a host kind
(`expo` | `web` | `tauri` | `electron` | `node`) or a platform
(`android` | `ios` | `macos` | `windows` | `linux`). A sequence runs on every
targeted host in parallel. It ends with a side-by-side table: a `device` row
with each host's strap binding, then per step each host's outcome and the
device that step reported; `*` marks the steps whose outcome differs between
hosts. `npm run driver -- <command>` in `example-expo` runs the same CLI.

### One strap per host in a sequence

A sequence may set `target` to a list (`["android", "ios"]`; on the command
line `--target android,ios`) and a `devices` map from a host id, host kind or
platform to a device name (exact, or a prefix ending in `*`):

```json
{ "target": ["android", "ios"], "devices": { "android": "Polar H10 E997042F", "ios": "Polar H10 E9B93D29" } }
```

For each host the most specific key wins: host id, then host kind, then
platform. The server adds that `device` to every `run` step whose command the
host describes as `acceptsDevice`, unless the step sets `args.device` itself
(the step wins). Commands without a device (`stop`, `force-disconnect`) are
left alone. When a sequence has `devices`, every selected host must resolve to
one: an unbound host fails the sequence with `sequence.device-unbound` before
anything runs, rather than letting it take whichever strap it finds first.
`sequences/parallel-two-straps.json` runs `h10-stream`, `device-info`, `mtu`,
`ecg` and `link-loss` on an Android and an iOS host at once, one strap each;
edit its `devices` keys for other hosts.

### Capturing H10 fingerprints (`h10-capture`)

The `h10-capture` scenario records a versioned JSON fingerprint of a strap
through the public API only: advertisement fields plus advertising interval
and RSSI stats, the full GATT database, every readable value, timing
distributions, behaviour probes and host metadata
(see [`tool/h10-sim`](../../tool/h10-sim/README.md) for the schema and the
equivalence check). The `capture` CLI command runs it on every targeted host
and saves each fingerprint:

```sh
node examples-shared/driver/server/cli.mjs capture android --device "Polar H10 E997042F"
node examples-shared/driver/server/cli.mjs capture all --device "Polar H10 E9B93D29" --out /tmp/h10
node examples-shared/driver/server/cli.mjs capture tauri --scan-ms 10000 --hr-ms 60000 --ecg-frames 30 --mtu 517
```

Files land in `fixtures/h10-fingerprints/<hostId>-<serial>-<date>.json`
(`--out` overrides the directory). A capture takes just over a minute with
defaults (10 s scan + 60 s HR stream + 30 ECG frames). The HR window must stay
at `--hr-ms 60000` or above on real straps; shorter windows are for the sim
and unit tests only.

Tonight's captures (server on the Mac, all hosts joined — check with `hosts`):

```sh
node examples-shared/driver/server/cli.mjs hosts
# Samsung (Expo Android) — strap E997042F, then strap E9B93D29:
node examples-shared/driver/server/cli.mjs capture <expo-android-host-id> --device "Polar H10 E997042F"
node examples-shared/driver/server/cli.mjs capture <expo-android-host-id> --device "Polar H10 E9B93D29"
# iPhone (Expo iOS) — strap E997042F, then strap E9B93D29:
node examples-shared/driver/server/cli.mjs capture <expo-ios-host-id> --device "Polar H10 E997042F"
node examples-shared/driver/server/cli.mjs capture <expo-ios-host-id> --device "Polar H10 E9B93D29"
# macOS Tauri host — strap E997042F, then strap E9B93D29:
node examples-shared/driver/server/cli.mjs capture <tauri-macos-host-id> --device "Polar H10 E997042F"
node examples-shared/driver/server/cli.mjs capture <tauri-macos-host-id> --device "Polar H10 E9B93D29"
```

Use the exact host ids from `hosts` (for example
`expo-android-google-pixel-9`). Six files, one per host per strap.

### Live dashboard (`live-dashboard`)

The `live-dashboard` scenario keeps one tile per Polar H10 in range: the
strap name, live heart rate with RR intervals and skin-contact state, a
downsampled PMD ECG trace (130 Hz, ~5 s window), battery level (180F/2A19)
and Device Information (180A firmware revision, model, serial). A tile
appears on the first scan observation and reconnects through an
application-owned `createConnectionSupervisor` when the strap drops out and
returns — the same code the example app's Live dashboard screen renders.

Unlike the single-strap scenarios it takes `devices` (plural), not `device`:
`"all-polar"` (the default, every Polar H10 in range) or a list of exact
advertised names. Commands: `start {devices?: "all-polar" | string[],
ecg?: boolean}`, `stop`, `snapshot`. The snapshot carries `tiles` (keyed by
peer id) and `tileOrder`; each tile reports its coarse `status`
(`discovered` | `connecting` | `streaming` | `reconnecting` | `lost` | `off`)
next to the library's own words (`supervisorState`, `lifecycleCause`,
lifecycle lines, typed error codes). Battery subscribes to notifications
where the library allows them and falls back to a periodic read where the
subscription is refused (`tile-battery-poll` announces the fallback with the
refusal code). Snapshot publishes stay throttled (250 ms) and the ECG ring
buffer is bounded (10 s), so the BLE delivery path is never blocked.

```sh
node examples-shared/driver/server/cli.mjs run android live-dashboard start '{"devices":"all-polar","ecg":true}'
node examples-shared/driver/server/cli.mjs run android live-dashboard start '{"devices":["Polar H10 E997042F"],"ecg":false}'
node examples-shared/driver/server/cli.mjs run android live-dashboard snapshot
node examples-shared/driver/server/cli.mjs run android live-dashboard stop
```

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

#### Apple TV (tvOS)

The same app and scenarios run on Apple TV from a generated stage,
`example-expo/ios-tv`, built with `EXPO_TV=1` (see
[`example-expo/README.md`](../../example-expo/README.md)). The phones keep
building from `example-expo/ios` and `example-expo/android`; the TV never
touches those directories. react-native-tvos keeps `Platform.OS === 'ios'`
and signals TV through `Platform.isTV`, so the adapter reports platform
`tvos` and backend `expo/tvos`: the control server sees a distinct host id
(`expo-tvos-<model>`) and `run tvos …` targets it. Scenario buttons are the
same `TouchableOpacity` controls, which the TV focus engine makes focusable
for the Siri Remote — no TV-only UI fork. The driver server stays shared on
port 8795; only Metro moves (the TV stage serves its own bundle).

### Android TV emulator host

boot/install/reverse/launch via `example-expo/scripts/android-tv-emu.sh all`
(AVD `TV_IMAGIBOOKS_GOOGLE_TV_API_36_arm64_v8a`); ceiling is a truthful
0-observation scan; connect attempts fail closed with
`operation.timed-out`; see receipt GTV1.md.

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

### Desktop hosts on macOS without clicks (`hosts.sh`)

`examples-shared/driver/hosts.sh up|down|status <tauri|electron|node>` starts
the desktop hosts idempotently: one PID file per host, one reusable Terminal
window titled `ubm-driver-hosts` (Terminal is the app macOS credits with the
Bluetooth permission), hosts detached, logs under `$TMPDIR/ubm-driver-hosts/`.
A second `up` is a no-op; `down all` stops every host, including ones started
by hand. Tauri opens the driver page directly (`UBM_TAURI_START_PAGE=driver.html`).

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
| `restoration` (`start` / `reconnect` / `restored`) | runs: associate, `presence.observe`, then `peers.restored` and a `when-available` reconnect with no scan | runs: `restoration.claim()`, then `peers.restored` and a direct reconnect with no scan | `capability.unsupported`: no background relaunch or presence wake; `restored` reports the owner's own answer | `capability.unsupported`: no OS restoration journal for a terminated app and no presence wake |
| `restoration` (`observe-presence` / `unobserve-presence`) | runs: arms `presence.observe` for the known peer id | `capability.unsupported` from the owner: Apple restores through `willRestoreState` and there is nothing to arm | no presence API (`scenario.presence-unavailable`); arm presence from an Expo/RN host | no presence API (`scenario.presence-unavailable`); arm presence from an Expo/RN host |

On Apple TV (`platform: tvos`) every scenario runs the same code as on the
iPhone, with two platform answers: tvOS has no background Bluetooth mode, so
`background acquire` answers `capability.unsupported` (the same refusal the
native Apple radio gives on iOS), and state restoration is unconfigured (the
TV prebuild writes no restoration keys), so `restoration.claim()` answers
`capability.unavailable`. Both are the library's own answers, never skips. Do not run
Bluetooth scenarios against hardware the owner has not made available; a
launch plus driver `hosts` plus the `readiness` report is the no-hardware
check.

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
cover the hub, the CLI client, sequences (including the per-target `devices`
binding) and the WebSocket. The scenario tests pin which call each scenario
makes: scan versus chooser, the gesture gate, the ECG order, the background
lease and app state, the `device` query and chooser filter, the single
`connect-retry`, and `stopAll`/`disposeDriver`. Each host also has an
adapter test:

- Expo: Metro resolution;
- Node: backend selection, identity and hub registration;
- Electron: the preload surface;
- Tauri: single-instance Vite resolution.
