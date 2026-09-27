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

| Path                                                     | What it is                                                                                                                           |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `protocol.ts`                                            | Wire contract `ubm-test-driver/1`, loaded by every host and by the server                                                            |
| `scenario-core.ts`                                       | `ScenarioController`, `ScenarioRegistry` (including `stopAll`), typed command arguments, console runtime                             |
| `scenarios/*.ts`                                         | `h10-stream`, `link-loss`, `device-info`, `mtu`, `scan-details`, `ecg`, `background`, `restoration`, `h10-capture`, `live-dashboard` |
| `polar-pmd.ts`                                           | Polar PMD ECG/ACC framing and H10 settings, from Polar's BLE SDK                                                                     |
| `host.ts`                                                | The host-adapter seam (`DriverHost`), peer acquisition, adapter readiness, capability lease                                          |
| `user-gesture.ts`                                        | The explicit pending-user-gesture gate (Web Bluetooth chooser)                                                                       |
| `remote-channel.ts`, `create-driver.ts`, `driver-url.ts` | Host → server channel, registry factory, `disposeDriver` (hot-reload teardown), URL rules                                            |
| `browser/`                                               | Shared by the Web, Tauri and Electron renderer hosts: WebSocket, visibility app state, the scenario panel                            |
| `server/`                                                | Control server and CLI (`cli.mjs`), hub, sequences                                                                                   |

A host adapter supplies only the manager factory and readiness step, the
platform label, the WebSocket, the app-state source and driver-URL discovery:

| Host              | Adapter                                                                                                                   | Manager                                                                               | App state               | WebSocket        | Driver URL                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | ----------------------- | ---------------- | ------------------------------------------------------------------------------------------ |
| Expo              | `example-expo/src/driver/app-driver.ts`                                                                                   | `createExpoBleManager` + readiness/permission                                         | React Native `AppState` | RN `WebSocket`   | Metro bundle host, or `EXPO_PUBLIC_UBM_DRIVER_URL`                                         |
| Expo (Apple TV)   | same adapter (`platform: tvos`, `backend: expo/tvos`)                                                                     | same                                                                                  | React Native `AppState` | RN `WebSocket`   | TV Metro bundle host, or `EXPO_PUBLIC_UBM_DRIVER_URL`                                      |
| Expo (Android TV) | same adapter (`platform: android`, `backend: expo/android`; phone APK installed as-is, launched via `adb shell am start`) | same                                                                                  | React Native `AppState` | RN `WebSocket`   | reversed Metro (emulator `localhost:8081` -> host `8082`), or `EXPO_PUBLIC_UBM_DRIVER_URL` |
| Web               | `example-web/src/driver.ts`                                                                                               | `createWebBleManager`                                                                 | page visibility         | browser          | page host, or `?driver=`                                                                   |
| Tauri             | `example-tauri/src/driver.ts`                                                                                             | `createTauriBleManager` (Tauri IPC)                                                   | page visibility         | browser          | local server, or `?driver=`                                                                |
| Electron          | `example-electron/driver/`                                                                                                | main: desktop provider + router/binding; renderer: `createElectronRendererBleManager` | page visibility         | browser          | local server, or `--driver-url`                                                            |
| Node              | `example-node/host.ts`                                                                                                    | `unified-ble-manager/node/{corebluetooth,winrt,bluez}`                                | none (`untracked`)      | Node `WebSocket` | local server, or `--driver-url` / `UBM_DRIVER_URL`                                         |

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

The background snapshot's `leaseState` retains that run's acquisition/API answer,
not live ownership after stopping. Inspect the `background.lease.release` cleanup
receipt for release success or retained failure. Starting another run resets the
answer to `null`; earlier acquisition events remain in the bounded event history.

### Choosing the strap: the `device` argument

Every peer-acquiring command (`h10-stream start`, `link-loss start`,
`device-info read`, `mtu probe`, `ecg start`, `background start`) takes an
optional `device` argument, so two hosts can each run against their own strap
at the same time:

| `device`                                               | `find()` query (`names`)                                   | Web chooser filter                                                                                                                |
| ------------------------------------------------------ | ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| absent                                                 | `prefixes: ["Polar H10"]` (the first H10 found, as before) | `localNamePrefix: "Polar H10"`                                                                                                    |
| `"Polar H10 A1B2C3D4"` (example exact advertised name) | `exact: ["Polar H10 A1B2C3D4"]`                            | `localNamePrefix: "Polar H10 A1B2C3D4"`, then the pick must be exactly that name or the run fails with `scenario.device-mismatch` |
| `"Polar H10 A1B*"` (example prefix, trailing `*`)      | `prefixes: ["Polar H10 A1B"]`                              | `localNamePrefix: "Polar H10 A1B"`                                                                                                |

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
argument, e.g. `h10-stream start '{"device":"SIM Polar H10 0001"}'`.

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

Stopping the dashboard cancels pending PMD response waits. Deliberate PMD aborts
use public typed BLE errors so the supervisor preserves their exact cause;
unrelated configuration exceptions are not relabelled from an abort signal.
An aborted settings
wait is reported as `operation.aborted` and does not advance into STOP/START.
If configuration is still unwinding, a stop can truthfully report
`connection-supervisor.late-configure-pending`; its cleanup owner is retained.
Retry stop after settlement and inspect the new receipt rather than treating
the first pending receipt as successful release. Cancellation of the response
wait does not establish that a previously accepted peripheral write was undone.

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
node examples-shared/driver/server/cli.mjs run android h10-stream start '{"device":"Polar H10 A1B2C3D4"}'
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
{ "target": ["android", "ios"], "devices": { "android": "Polar H10 A1B2C3D4", "ios": "Polar H10 E5F6A7B8" } }
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
node examples-shared/driver/server/cli.mjs capture android --device "Polar H10 A1B2C3D4"
node examples-shared/driver/server/cli.mjs capture all --device "Polar H10 E5F6A7B8" --out /tmp/h10
node examples-shared/driver/server/cli.mjs capture tauri --scan-ms 10000 --hr-ms 60000 --ecg-frames 30 --mtu 517
```

Files land in `fixtures/h10-fingerprints/<hostId>-<serial>-<date>.json`
(`--out` overrides the directory). A capture takes just over a minute with
defaults (10 s scan + 60 s HR stream + 30 ECG frames). The HR window must stay
at `--hr-ms 60000` or above on real straps; shorter windows are for the sim
and unit tests only.

For a repeatable multi-host capture, first obtain the current host IDs, then
capture each assigned strap explicitly. Keep the resulting records outside the
repository until they are validated and retained as release evidence:

```sh
node examples-shared/driver/server/cli.mjs hosts
node examples-shared/driver/server/cli.mjs capture <host-id> --device "Polar H10 <advertised-suffix>" --out /tmp/ubm-h10-captures
```

### Live dashboard (`live-dashboard`)

ECG and ACC are independently selectable. ACC accepts `acc: true`,
`accSampleRateHz: 25 | 50 | 100 | 200` (default 200), and
`accRangeG: 2 | 4 | 8` (default 8), with fixed 16-bit XYZ samples in milli-g.
Both streams share the device's PMD control/data channels. Failed commands,
malformed frames and stream loss stay visible rather than becoming empty traces.

#### Record and compare a simulator with a real H10

1. Stop any existing run, choose one exact device and use the same ECG/ACC
   settings for both captures. Give each recording a meaningful `label` and
   `notes` (simulator profile/source revision or H10 firmware, posture/motion).
2. Run `record-start` **before** `start` to include initial commands and settings.
   Recording can also start during a run, but cannot reconstruct earlier packets.
3. Run the desired interval, then `stop` to retain cleanup commands and stop the
   recording. `record-stop` stops only recording while live streams continue.
4. `record-export` returns `ubm-pmd-recording/1` JSON. Browser panels request a
   JSON download; Expo writes a local document then opens the native share sheet.
   Remote driver callers receive the full artifact only on this explicit command.
   Export before `record-clear`; clearing explicitly discards the in-memory capture.

Example command arguments for `live-dashboard`:

```json
{"command":"record-start","args":{"label":"sim-200hz-8g","notes":"stationary synthetic fixture"}}
{"command":"start","args":{"devices":["SIM Polar H10 0001"],"ecg":true,"acc":true,"accSampleRateHz":200,"accRangeG":8}}
```

Captures retain raw packet hex, exact sensor timestamps as decimal strings,
host monotonic receipt times, peer/connection/PMD generations, selected settings,
discovered device information, and explicit errors/loss. They are bounded to
20,000 records and 8 MiB of serialized metadata/record payloads (JSON envelope
and in-memory overhead are additional). At capacity, retained records stop
growing and every omitted record is counted; the capture is marked incomplete.
No packets are silently overwritten. Stop before clearing a capacity-limited run.

Metadata `optionsAtRecordingStart` and `peersAtRecordingStart` describe only the
instant `record-start` was called. Before acquisition or after stop they are
`null` and `{}`, respectively; subsequent setup, settings and device identities
are retained in chronological records.

The recorder is opt-in, in-memory and observes this JS host only. App/process
termination loses unexported data. It is **not** native durable/background
recording; the native continuation outbox is a separate mechanism. Exports
contain device identifiers and physiological data: keep them private unless
you explicitly choose to share them. Bulk packet data is not mirrored into
automatic snapshots or command-result history.

The live dashboard's foreground connection supervisor reissues PMD setup/start
commands after reconnect. The separate `continuation` scenario uses the shared
H10 recipe to declare generic native setup commands: subscribe to HR, PMD
control and data, then issue correlated STOP/START commands for the selected
ECG/ACC measurements after each recovered generation. Merely resubscribing to
HR is still not evidence of successful background ECG/ACC recovery; inspect
the setup outcome and positive recorded PMD values from the new generation.

### Native continuation and durable recording

The separate `process-continuation` scenario uses an optional trusted host
controller. Its snapshot `owned` reports this scenario's local cleanup
obligation: initially `null` (unqueried), `true` before native execute or claim
settles and after uncertain/refused cleanup, and `false` after confirmed disposal
or an empty claim followed by actual null status. It does not prove native
acquisition or global engine absence; use `status` for the process owner's answer.
Concurrent status reads never erase a pending local cleanup obligation. The
controller runs on the **already-owned process central**. It does not persist an OS
wake declaration, select a backend, open another manager, or promise recovery
after host-process exit. Hosts without this seam report `capability.unsupported`;
the ordinary desktop factory's OS-wake refusal remains unchanged.

Commands use the same driver `run <scenario> <command> --args <JSON>` transport:

```text
process-continuation execute {"peerId":"<exact observed identity>","measurements":"hr-ecg-acc","sampleRateHz":50,"rangeG":4,"recordingId":"h10_process","maxBytes":16777216,"maxRecords":100000}
process-continuation status {}
process-continuation claim {}
process-continuation recording-prepare {"recordingId":"h10_process"}
process-continuation recording-acknowledge {"recordingId":"h10_process","token":"<saved batch token>"}
```

`execute` requires an explicit peer identity and uses the same H10 setup recipe
as mobile continuation. `claim` and `stop` return decoded volatile values
(bytes use the driver's hex representation), loss facts and the owner's
disposal result. Save that response: a failed disposal is retryable and does
not authorize another execution. Registry shutdown also claims an existing
process owner after renderer reload. An empty untouched-engine claim may report
`disposed: false`; only a subsequent actual `status: null` establishes that
there is no session to clean up, without rewriting the claim result.
`last-claim` exposes its latest retained
handoff. Normal command event history contains counts, not sensor payloads.
The registry's final shutdown receipt includes the full decoded handoff (also
when disposal fails), so a finite CLI does not discard its data at exit. Save
that output privately; it can contain sensor records.

Offline `recording-status`, `recording-prepare`, `recording-acknowledge`,
`recording-stop` and `recording-clear` never acquire a radio. They reuse the
mobile scenario's explicit journal commands. Preparing does not acknowledge;
stopping journal admission does not stop native collection; only explicit
`recording-clear` deletes a stopped journal. The trusted host supplies private
storage configuration: commands accept recording IDs and quotas, never paths.

On a host exposing the public continuation factory, `continuation/declare`
persists an actual standing order rather than an intent in the driver's UI.
Unsupported hosts refuse the operation. Validate the selected device's PMD
features and settings in the foreground first. For example, send this command
to the `continuation` scenario using the exact peer identity discovered there:

```json
{"command":"declare","args":{"onAppearance":"native","peerId":"<exact-peer-id>","measurements":"hr-ecg-acc","sampleRateHz":200,"rangeG":8,"recordingId":"h10_background_run_1","maxBytes":16777216,"maxRecords":100000}}
```

The recipe supports HR, HR+ECG, HR+ACC and HR+ECG+ACC, all H10 ACC rates
(25/50/100/200 Hz) and ranges (2/4/8 g), at 16-bit resolution. The host's
restoration/presence setup and permissions remain necessary; declaration alone
does not manufacture an OS wake. See [background lifecycle and platform
limits](../../docs/BACKGROUND.md). Native setup is protocol-neutral in UBM;
the Polar command construction stays in the shared reference app.

Durable recording is opt-in with explicit quotas. Unlike the live dashboard's
JS recorder, it retains native records independently of the app's JS runtime
and native claim cursor. Use `recording-status` with `recordingId`, then
`recording-prepare` with that ID and bounded `maxItems`/`maxBytes`. Prepare
returns records and a token without consuming them. Save or process the full
result before calling `recording-acknowledge` with the same ID and token.
Repeated prepare before acknowledgement returns the same prefix. Automatic
command history contains counts, not the sensor payload or cursor token.

`recording-stop` closes recording admission but does not release a radio or
disarm the standing order. First run `backlog` and verify successful native radio disposal;
a refused claim remains owned and must be retried. Then use scenario `stop` to persist
`record-only`; neither operation clears the retained journal. Its data remains available through the offline recording
commands. `recording-clear` explicitly deletes a stopped recording. Storage is
app-private but not encrypted by UBM; retain exports privately. These controls
use the public offline API and do not require a live BLE session.

Android's alternate `headless-task` declaration requires an app-registered
`headlessTaskName`. `foreground-service` requires
`foregroundService.notification` with `channelId`, `channelName` and `title`
(optional `body` and `icon`). A dispatched task or started service is not proof
that application work completed. These strategies do not automatically select
the native PMD recipe or durable collector; choose `native` for that standing
order. Report actual platform refusal rather than treating every strategy as
available on every host.

The Expo Android reference app exposes `continuation/headless-history` with no
arguments to read its bounded task-receipt summaries without opening BLE. A
`completed` receipt with a battery result and released cleanup is separate proof
from native `task-dispatched`. Peer identifiers and raw error payloads are omitted;
malformed history or storage failure rejects, and unsupported hosts report
`capability.unsupported`. This diagnostic does not consume the native journal.

Compare exported files locally with Node 22.18+:

```sh
node examples-shared/driver/compare-pmd-recordings.mjs simulator.json real-h10.json > comparison.json
```

The tool re-decodes raw bytes using the live parser, groups by peer, connection,
PMD session, measurement and settings, and reports sample counts, sensor-clock
rate, per-axis min/max/mean, decode errors and recorded loss. Timestamp
discontinuities use a half-sample-period tolerance and never cross generations.
Match the settings before comparing. For a real H10, first record stationary
orientations (gravity near 1000 milli-g on the relevant axis), then controlled
motion; repeat every rate/range pair and both ECG/ACC stop orders. Synthetic
waveforms and different physical motion are not expected to match byte-for-byte.
The report explicitly does **not** establish device equivalence or hardware
qualification; real-device captures remain required.

The `live-dashboard` scenario keeps one tile per Polar H10 in range: the
strap name, live heart rate with RR intervals and skin-contact state, a
downsampled PMD ECG trace (130 Hz, ~5 s window), battery level (180F/2A19)
and Device Information (180A firmware revision, model, serial). A tile
appears from a matching already-connected peer in the public peer directory or
the first scan observation, and reconnects through an
application-owned `createConnectionSupervisor` when the strap drops out and
returns — the same code the example app's Live dashboard screen renders.
On scanning hosts the dashboard first queries connected peers without a service
filter, applies the same explicit name policy, and then scans for other straps.
GATT discovery/setup remains authoritative for the actual profile. This permits
a second dashboard owner to join a strap that stopped advertising while another
owner holds its link. Duplicate directory/scan observations create only one tile.
An unsupported directory is reported explicitly and scanning continues; genuine
directory errors fail the start. The query is bounded and stopped/late results
cannot admit a tile. Chooser-only hosts retain their user-gesture chooser path.

Unlike the single-strap scenarios it takes `devices` (plural), not `device`:
`"all-polar"` (the default, every Polar H10 in range) or a list of exact
advertised names. Commands: `start {devices?: "all-polar" | string[],
ecg?: boolean}`, `stop`, `snapshot`. The snapshot carries `tiles` (keyed by
peer id) and `tileOrder`; each tile reports its coarse `status`
(`discovered` | `connecting` | `streaming` | `reconnecting` | `lost` | `failed` | `off`)
next to the library's own words (`supervisorState`, `lifecycleCause`,
lifecycle lines, typed error codes). The tile retains the precise configuration
failure when a supervisor stops with an error; successful recovery clears it.
Cleanup events include the complete structured receipt rather than only its state.
Battery subscribes to notifications
where the library allows them and falls back to a periodic read where the
subscription is refused (`tile-battery-poll` announces the fallback with the
refusal code). Snapshot publishes stay throttled (250 ms) and the ECG ring
buffer is bounded (10 s), so the BLE delivery path is never blocked.

```sh
node examples-shared/driver/server/cli.mjs run android live-dashboard start '{"devices":"all-polar","ecg":true}'
node examples-shared/driver/server/cli.mjs run android live-dashboard start '{"devices":["Polar H10 A1B2C3D4"],"ecg":false}'
node examples-shared/driver/server/cli.mjs run android live-dashboard snapshot
node examples-shared/driver/server/cli.mjs run android live-dashboard stop
```

## Launching each host

Every host below except Expo runs from the repository root after `pnpm prepack`,
because it imports the checkout's own built package. Expo installs the same
packed checkout through its `file:..` dependency; its iOS command refreshes and
verifies the copied RustCore before invoking Xcode.

### Expo (Android, iOS)

```sh
pnpm prepack
pnpm --dir example-expo install --no-frozen-lockfile
pnpm --dir example-expo android                                     # or: pnpm --dir example-expo ios
adb reverse tcp:8795 tcp:8795                                       # Android over USB with Metro on localhost
```

`pnpm --dir example-expo ios` is the supported local iOS entrypoint. It
refreshes the checked-out package copy when its Apple native artifact is stale,
checks the copied framework identity, and verifies the generated restoration
configuration before Xcode builds. Run `expo prebuild --clean --no-install`
only when intentionally regenerating native projects; run the `ios` command
afterward so the same checks protect the build.

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

`scan-details` and the `h10-capture` advertisement stage pass their requested
duration to `scan({ timeoutMs })` as well as scheduling the reference timer.
Fractional milliseconds are rounded down for the library deadline. The native
mobile owner can end the scan while JavaScript timers are suspended; the command
result and explicit cleanup still wait for JavaScript to resume. Finite expiry
is reported as the scan's terminal reason, not invented observation evidence.
A completed command (including an advertisement stage's `ok: true`) with zero
observations does not establish that advertisements were received or qualify a
radio; inspect the counts, terminal/error events, and cleanup receipts.
H10 advertisement capture emits overflow and terminal notices with their loss
counters. Source-failure terminals mark that stage failed with the original
structured cause; finite expiry and owner release remain normal endings. A
refused scan cleanup blocks the next peer-find scan and remains retryable through
Stop. If iteration itself throws and cleanup also fails, both original source
and cleanup details remain in the failure. Timer-stop refusals remain diagnostic
events even if a later retry releases the scan.

The library answers each call itself. The rows below are what the source says
to expect. They are not hardware evidence.

| Scenario                                                   | Expo Android                                                                                             | Expo iOS                                                                                                             | Web                                                                                                          | Tauri / Electron / Node (desktop core)                                                                                                                                                                                             |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `h10-stream`, `device-info`                                | runs                                                                                                     | runs                                                                                                                 | runs after the chooser click                                                                                 | runs                                                                                                                                                                                                                               |
| `link-loss`                                                | runs (reconnect by peer reference)                                                                       | runs                                                                                                                 | runs after the chooser click                                                                                 | runs                                                                                                                                                                                                                               |
| `h10-stream` / `link-loss` with `intent: "when-available"` | runs                                                                                                     | `capability.unsupported` from `connect`                                                                              | answered by `connect`                                                                                        | answered by `connect`                                                                                                                                                                                                              |
| `mtu`                                                      | runs                                                                                                     | `requestMtu`, `effectiveMtu`, `readPhy` report unsupported (CoreBluetooth negotiates the MTU); write length measured | each probe reports the backend's answer                                                                      | each probe reports the backend's answer (macOS: CoreBluetooth rows unsupported)                                                                                                                                                    |
| `scan-details`                                             | runs                                                                                                     | runs                                                                                                                 | `scan()` refuses: Web Bluetooth has no continuous scan (`web:continuous-scan` unsupported)                   | runs                                                                                                                                                                                                                               |
| `ecg`                                                      | runs                                                                                                     | reads the PMD control point while it is notifying, which exercises the library's read-while-notifying path           | runs after the chooser click (PMD is in `optionalServices`)                                                  | runs                                                                                                                                                                                                                               |
| `background`                                               | Expo lease API; app state from `AppState`                                                                | same                                                                                                                 | lease: `web:background-operation` descriptor (unsupported); app state from page visibility                   | lease: `background:desktop-maintain-connection` descriptor (registered on WinRT only); Tauri/Electron use page visibility; Node reports `untracked` (a CLI has no app lifecycle), so `sequences/background.json` cannot pass there |
| `restoration` (`start` / `reconnect` / `restored`)         | runs: associate, `presence.observe`, then `peers.restored` and a `when-available` reconnect with no scan | runs: `restoration.claim()`, then `peers.restored` and a direct reconnect with no scan                               | `capability.unsupported`: no background relaunch or presence wake; `restored` reports the owner's own answer | `capability.unsupported`: no OS restoration journal for a terminated app and no presence wake                                                                                                                                      |
| `restoration` (`observe-presence` / `unobserve-presence`)  | runs: arms `presence.observe` for the known peer id                                                      | `capability.unsupported` from the owner: Apple restores through `willRestoreState` and there is nothing to arm       | no presence API (`scenario.presence-unavailable`); arm presence from an Expo/RN host                         | no presence API (`scenario.presence-unavailable`); arm presence from an Expo/RN host                                                                                                                                               |

On Android, `restoration associate {"name":"SIM Polar H10 0001"}` opens the
actual system consent chooser for that exact supplied name (no wildcard or
hardcoded strap). It leaves existing associations untouched. The chooser wait is
given a 60-second deadline; cancellation and platform refusal propagate unchanged.
Android may suspend JS timers while its chooser covers the app. An accepted
result arriving after the elapsed deadline still reports the actual association
and includes `timing: {state: "deadline-expired", budgetMs, elapsedMs,
followUp: "caller-decides"}`. It never claims the deadline cancelled OS work.
Manager cleanup is explicit and a failed release remains owned for retry via
`restoration stop`. Late system acceptance is reported, not undone or hidden;
the caller decides whether to keep or explicitly remove the association.
Then arm `restoration observe-presence` using the returned exact peer ID.
Association, presence observe/unobserve, and restored-peer queries are one-shot
commands: their temporary manager is released before success returns. Observation
itself remains owned by the platform until `unobserve-presence`; no intervening
`stop` is required. Failed temporary-manager cleanup remains retryable via `stop`.
`continuation backlog` retains the owner's stream-end reasons/counters,
`afterCutoffLoss`, durable recording reference and disposal failure separately.
It summarizes sensor values without logging their bytes; zero journal loss does
not imply zero volatile handoff loss or uninterrupted peripheral sampling.
Remote-driver qualification uses a custom native Debug app; Release intentionally
disables that development control surface. Label evidence accordingly.

After iOS relaunch, run `restoration restored`, then pass a returned peer's
`reference` to `restoration reconnect` as `peerReference` with `intent: "direct"`.
For Android's presence path, likewise pass the returned durable `reference` as
`peerReference`, with explicit `intent: "when-available"`. This command creates a
fresh manager and rejects manager-local `peerId` strings; the physical iOS
fresh-manager direct-reconnect qualification remains open.

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
pnpm test:driver                 # frozen Expo consumer install + shared/server/host tests

pnpm typecheck:references        # after prepack: shared, Node, web, Tauri, Electron
pnpm typecheck:references:expo   # after the separate Expo dependency install
```

Linux Node22 package CI and clean preflight reuse the root reference command
after building public package imports. Expo has a separate dependency tree, so
its canonical command runs in the existing Expo CI/Android-preflight lane after
install and SDK alignment. `preflight.sh --fast` (or missing Android SDK/JDK)
skips that Expo typecheck with the Android lane; it is not all-host typecheck proof.
Both gates also run the canonical `test:driver` suite, including Tauri and
Electron behavior regressions. After `pnpm prepack`, the root command refreshes
Expo's actual `file:..` consumer with a forced, frozen-lockfile install before
running the example's unchanged test globs. This prerequisite also supplies
Expo's runtime resolver for the package-resolution regressions; it does not
prebuild an app or run SDK alignment. The example-local `test:driver` command
alone assumes that consumer install is already current.

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
