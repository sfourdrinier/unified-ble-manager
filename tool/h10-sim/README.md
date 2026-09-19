<!-- tool/h10-sim/README.md -->

# h10-sim — Polar H10 BLE peripheral simulator

A test tool (not part of the published package) that impersonates a Polar H10
strap so the shared driver scenarios (`examples-shared/driver/scenarios`:
`h10-stream`, `device-info`, `mtu`, `ecg`, `link-loss`, `background`,
`scan-details`) can run on every host without a real strap.

It has its own Cargo workspace (`tool/h10-sim/Cargo.toml`) and is outside the
root Cargo workspace and the npm `files` allowlist, so it never ships in the
package. No hardware is needed to build or test it; only running the radio
needs a Bluetooth adapter.

Identity and state come from one JSON device profile
(`profiles/stock-h10.json` is compiled in as the default;
`profiles/low-battery-legacy.json` is a low-battery, older-firmware unit):
`--profile` picks one, CLI flags override single fields, and the control
port / driver commands change everything live. See Device profiles below.

## Peripheral backend: crate choice

One `PeripheralRadio` trait (`src/radio.rs`), one backend per platform:

| Platform | Backend | Version |
| --- | --- | --- |
| macOS | `ble-peripheral-rust` 0.2.0 (CoreBluetooth `CBPeripheralManager` via `objc2-core-bluetooth`) | 0.2.2 |
| Linux | `bluer` directly (BlueZ GATT server + `LEAdvertisement1`) | 0.17.4 |
| Windows | `ble-peripheral-rust` 0.2.0 (WinRT `GattServiceProvider` via `windows`) | 0.57 |

Linux drives `bluer` directly (`src/bluer_radio.rs`) instead of going through
[`ble-peripheral-rust =0.2.0`](https://crates.io/crates/ble-peripheral-rust)
(MIT, single maintainer, last release 2024-12-28): that crate hardcodes its
BlueZ advertisement object to name plus service UUIDs with no control over
`Includes`, `Appearance`, `TxPower`, duration or intervals, and it registers
the advertisement *before* the GATT application. The direct backend registers
byte-identical advertisement data (see Advertisement layout below) but builds
every `LEAdvertisement1` property in the sim, registers the GATT application
first, and names the stage that rejects a registration (`serve GATT
application` vs `register advertisement`). Read/write/notify plumbing mirrors
that crate's BlueZ backend against the same `bluer` version, so over-the-air
behaviour is unchanged.

Deliberately not used:

- `objc2-core-bluetooth` 0.3.2 (actively maintained Apple bindings) — raw
  bindings; a peripheral needs a hand-written delegate plus run-loop pump, and
  still nothing for Linux.
- `btleplug` 0.11 — central role only, no peripheral mode.
- `bluest` — no GATT server support on macOS.

If `ble-peripheral-rust` ever stops covering macOS well, implement
`PeripheralRadio` with a thin `objc2` `CBPeripheralManager` wrapper there;
`src/sim.rs` does not change.

## Advertisement layout

BlueZ 5.72 (`src/advertising.c`) lays the sim's `LEAdvertisement1` object
(Type `peripheral`, ServiceUUIDs `180D` + `FEEE`, `LocalName`,
`Discoverable`, empty `Includes`) out as two legacy payloads:

- advertisement data (9 bytes): Flags `02 01 06` plus the complete 16-bit
  service-UUID list — the name never competes for these 31 bytes;
- scan response (`2 + name.len()` bytes): the complete local name, exactly
  like a real H10.

`src/advertisement.rs` pins this layout and the budget: `advertisement_sizes`
computes both lengths, `fit_name` truncates `--name` to the 29 bytes that fit
the scan response (UTF-8-boundary safe, logged as `advertising-name-truncated`
— never silent), and `ADVERTISED_SERVICES` pins the advertised set so the
128-bit PMD service UUID or manufacturer data can never silently spend the
name's budget. Unit tests cover the default name, the 17-character Linux
name, a 29-character maximum and an over-long name.

## Simulated GATT surface

Advertised name `Polar H10 SIM<4 hex>` (default `Polar H10 SIM0001`,
`--name` overrides), advertising Heart Rate (`180D`) and Polar (`FEEE`).
A real H10 also advertises Polar manufacturer data (company `0x006B`); the
peripheral APIs used here cannot emit manufacturer data (on Apple platforms
that is an OS limitation), so the sim sends none — see fidelity gaps.

| Service | Characteristics | Properties |
| --- | --- | --- |
| Heart Rate `180D` | `2A37` measurement: notify ~1 Hz, flags `0x16` (uint8 bpm, contact detected, RR present), 1 RR interval | notify |
| | `2A38` body sensor location: chest (`1`) | read |
| Battery `180F` | `2A19` level (default 85%) | read, notify (60 s) |
| Device Information `180A` | `2A29` manufacturer `Polar Electro Oy`, `2A24` model `H10`, `2A25` serial, `2A26` firmware, `2A27` hardware, `2A28` software, `2A23` system id | read |
| Polar PMD `FB005C80-…` | `FB005C81` control point: read returns features (ECG); write `0x01` get-settings / `0x02` start / `0x03` stop, each answered with an indicate `[0xF0, op, type, status, more, params…]` | read, write, indicate |
| | `FB005C82` data: ECG frames at ~130 samples/s (`[0x00, timestampNs u64 LE, 0x00, samples…]`, signed 24-bit LE µV, deterministic synthetic PQRST) | notify |

No PnP ID (`2A50`), like the real H10 — `device-info read` reports that read
as its own failed outcome. Start commands must request 130 Hz / 14 bit;
anything else is refused with the SDK status codes (`ERROR_INVALID_SAMPLE_RATE`
0x08, `ERROR_INVALID_RESOLUTION` 0x07). Valid Polar types the H10 lacks
(PPG/ACC/PPI) answer `ERROR_NOT_SUPPORTED` (0x03).

Byte layouts follow the Polar BLE SDK source
([`polarofficial/polar-ble-sdk`](https://github.com/polarofficial/polar-ble-sdk):
`BlePMDClient`, `PmdControlPointResponse`, `PmdDataFrame`,
`PmdMeasurementType.fromByteArray`, `PmdSetting.typeToFieldSize`) and the
Bluetooth SIG Heart Rate, Battery and Device Information service
specifications ([spec index](https://www.bluetooth.com/specifications/specs/)).

## Build

```sh
cd tool/h10-sim
cargo build        # binary: target/debug/h10-sim (set CARGO_TARGET_DIR to redirect)
cargo test         # 57 unit tests on macOS/Windows, 67 on Linux (see Tests below)
node tests/xcheck/run-xcheck.cjs   # run from the repo root; see Tests below
cargo clippy --all-targets   # must stay warning-free
cargo fmt --check
```

Linux builds need `libdbus-1-dev` (the `bluer` backend binds `libdbus-1`).
Cross-checking from macOS
(`cargo check --target x86_64-unknown-linux-gnu`, target is installed) stops
in the `libdbus-sys` build script without a Linux sysroot providing it;
re-run the Linux check on a Linux host or with a sysroot. The Linux-only
module (`src/bluer_radio.rs`) is not compiled on macOS at all, so a Linux
`cargo build` plus `cargo test` is required after touching it.

## Run

```sh
./target/debug/h10-sim [--profile profiles/low-battery-legacy.json] [--name "Polar H10 SIM0001"]
  [--bpm 72] [--battery 85] [--pair-policy just-works] [--ecg-file ecg.txt]
  [--control-bind 127.0.0.1] [--control-port 17935] [--control-token-file token.txt]
  [--driver ws://127.0.0.1:8795/host]
```

Later flags win: `--profile` applies first, then `--name`/`--bpm`/`--battery`.

- **macOS: launch from Terminal.app** (or whichever terminal owns the window),
  because Bluetooth permission follows the launching process. The sim waits up
  to 30 s for a powered adapter and fails loudly otherwise.
- **Linux:** see Linux requirements below.

## Linux requirements

- `bluetoothd` running (BlueZ 5.72 verified), adapter powered; build needs
  `libdbus-1-dev` (`sudo apt install libdbus-1-dev`).
- Known identity: the default profile advertises `Polar H10 SIM0001` with
  serial `SIM000001`, firmware 3.2.1 and System ID manufacturer 1 / OUI
  `6B:00:00`. The radio address is the controller's own public address, not
  the sim's to choose — read it with `bluetoothctl show` (controller
  `90:DE:80:3B:69:78` on the reference host) and document it beside the
  profile. Driver scenarios target the sim by name (`device: "Polar H10
  SIM0001"`), so the address never enters a command; use the address only to
  confirm over the air (e.g. in `btmon` or `bluetoothctl devices`) that the
  peer you found is this host.
- D-Bus access to `org.bluez`: run as root or as a user in the `bluetooth`
  group (check `groups`; `sudo usermod -aG bluetooth $USER` then log back in).
  An access denial surfaces immediately as a D-Bus `AccessDenied` error, not
  as a registration failure.
- Stop any other advertiser first (`bluetoothctl advertise clear`, companion
  apps, a previous sim still running) — instances are per-registration and a
  stale owner confuses the diagnosis.
- Names longer than 29 bytes are truncated to fit the scan response (logged
  as `advertising-name-truncated`); keep `--name` at or under 29 bytes so the
  driver `device` match sees the full name.
- Log reading: `services-registered` is local bookkeeping; on Linux the BlueZ
  GATT application registers next (the backend reports which stage fails), so
  a `register advertisement` failure means the advertisement object reached
  `bluetoothd` and the kernel rejected it — D-Bus delivery itself worked.
- A `register advertisement` failure carries the adapter's
  `LEAdvertisingManager1` counters read just before registering
  (`ActiveInstances=… SupportedInstances=…`). Before changing the sim, check
  whether the host can advertise at all: `bluetoothctl`, `menu advertise`,
  `back`, `advertise on` with no sim running. If that empty advertisement
  fails with the same `add_client_complete() … Invalid Parameters (0x0d)` in
  `journalctl -u bluetooth --since -5min`, the rejection is host state, not an
  `LEAdvertisement1` property: on the reference host every property
  combination was bisected (none, each of 180D/FEEE/name/Discoverable
  on/off/manufacturer data, Includes local-name/tx-power, Appearance,
  intervals, TxPower, SecondaryChannel, DiscoverableTimeout, `broadcast`
  type) and all failed identically, `bluetoothctl` included.
- `ActiveInstances` counts registrations `bluetoothd` still holds. BlueZ
  5.72 installs the owner-disconnect watch only once a registration
  completes, so a registration whose owner never answered the property fetch
  is never released and keeps its instance id, and every new registration is
  given the next id up. Freeing them needs root: `sudo systemctl restart
  bluetooth` (drops every registration on the host). To see the HCI status
  behind `0x0d`, capture `sudo btmon` during a registration — it shows
  whether `LE Set Extended Advertising Parameters` or `… Data` was rejected
  and with which handle. The sim never escalates to do either.

Every GATT event (subscribe, read, write, PMD command, notify) is logged to
stdout as one JSON object per line with `seq`, `ts` (RFC 3339 UTC) and `kind`.
Malformed writes and failed operations are logged with `error`/`reason`
fields — nothing is dropped silently.

## Control protocol

JSON-lines TCP on `127.0.0.1:17935` (see `--control-bind` / `--control-port`).
One object per line in, one `{"ok":…}` reply per line out. Example session:

```sh
printf '{"cmd":"set-bpm","bpm":96}\n' | nc 127.0.0.1 17935
printf '{"cmd":"reject-next-pmd","status":3}\n' | nc 127.0.0.1 17935
printf '{"cmd":"get-state"}\n' | nc 127.0.0.1 17935
```

| Command | Effect |
| --- | --- |
| `{"cmd":"set-bpm","bpm":96}` | Heart rate for HR notifies and the ECG waveform (clears a scripted curve) |
| `{"cmd":"set-battery","level":15}` | Battery level now (0–100, notified immediately) |
| `{"cmd":"set-contact","detected":false}` | Sensor-contact lost/detected (HR flags `0x04`/`0x06`) |
| `{"cmd":"pair-policy","policy":"disabled"}` | Pairing policy `just-works`/`disabled` |
| `{"cmd":"load-profile","path":"…"}` | Load a profile file live (re-advertises when advertising) |
| `{"cmd":"set-advertising","on":false}` | Stop/start advertising |
| `{"cmd":"drop-link"}` | Stop advertising, halt ECG, and disconnect centrals (BlueZ `Device1.Disconnect`, counted in the reply; on CoreBluetooth a connected central stays connected — no disconnect API) |
| `{"cmd":"set-silent","on":true}` | Stop notifying while keeping the link up |
| `{"cmd":"reject-next-pmd","status":3}` | Fail the next PMD command with a status code, then clear |
| `{"cmd":"clear-pmd-fault"}` | Disarm without firing |
| `{"cmd":"set-rates","hrHz":2.0,"ecgFramesPerSec":4.0,"ecgFrameSamples":65}` | Stream rates (`hrHz` 0.1–10, `ecgFramesPerSec` 0.5–10, samples 1–167 so a frame fits MTU 512) |
| `{"cmd":"get-state"}` | Current state snapshot |
| `{"cmd":"help"}` | Command list (generated from the same table the driver hello uses) |

Unknown commands and out-of-range values get `{"ok":false,"error":"…"}`.

### Authentication

`--control-bind` defaults to `127.0.0.1`. The token comes from `--control-token`,
`--control-token-file`, or the `H10SIM_TOKEN` environment variable (that
precedence order); a token file is trimmed and an unreadable one fails startup
loudly. When a token is set, every connection's first line must equal it —
a mismatch gets `{"ok":false,"error":"auth failed"}` and the connection
closes. The presented value is never logged (only `control-auth-failed` with
the peer address). Binding a non-loopback address without a token refuses to
start. Off-loopback operation is LAN-trusted-only: there is no TLS, so use a
token and a trusted network (or an SSH tunnel and keep the loopback bind).

## Device profiles

One JSON file defines every identity and state field (see
`profiles/stock-h10.json` for the full schema): Device Information strings
plus System ID parts, battery level and drain, advertising name and
manufacturer data, heart rate plus contact/RR-jitter/curve, PMD pair policy
and ECG source. `profiles/low-battery-legacy.json` is a second unit (15%,
firmware 1.5.9, serial `SIM000042`, draining 1%/min with a 68→96 bpm curve)
so tests can pick one. A bad path, bad JSON, out-of-range battery or bad hex
fails loudly naming the file — never a silent fallback.

Battery drain (`drain_per_min`) notifies `2A19` whenever the level changes
(the 60 s heartbeat stays regardless). RR jitter is a deterministic sine of
the beat index (period 10), so identical runs produce identical bytes. The
BPM curve holds each step's bpm from its `at_s` on; an explicit `set-bpm`
clears the curve. `--ecg-file` (or `"ecg_source": {"file": …}`) replays a
recorded trace: text, one integer µV per line at 130 Hz, cycling forever; a
missing file or a bad line fails startup with its line number. The H10 `2A37`
flags carry no energy-expended bit, so energy expended is intentionally not
simulated.

## Pairing and bonding

Default policy `just-works`: bonding works when the central requests it and
nothing is gated on a bond — PMD streams unbonded, like the real H10 (the
Polar SDK performs no bonding step before PMD start). Bonding itself is
completed by BlueZ plus the host agent (pair from `bluetoothctl` with its
default agent); the sim neither requests nor blocks it. `--pair-policy
disabled` (or the `pair-policy` command) records the refusal policy in state
and `get-state`; a BlueZ-level pairing refusal is not something a GATT
application can enforce, so the flag is policy documentation plus the future
enforcement point. No privilege escalation is involved anywhere: everything
uses the caller's own D-Bus session.

## Pointing driver scenarios at the sim

Every peer-acquiring command takes the `device` argument (exact advertised
name, or a prefix ending in `*`; default `Polar H10*`):

- `h10-stream start '{"device":"Polar H10 SIM0001"}'` — HR stream; drive with `set-bpm`.
- `device-info read '{"device":"Polar H10 SIM0001"}'` — DIS + battery reads.
- `mtu probe '{"mtu":517,"device":"Polar H10 SIM0001"}'` — MTU is the
  platform's own answer; the sim cannot raise it.
- `ecg start '{"mtu":517,"device":"Polar H10 SIM0001"}'` — PMD ECG at 130 Hz.
- `link-loss start '{"device":"Polar H10 SIM0001"}'` — combine with
  `set-silent` (no data, link up) and `drop-link` (BlueZ tears the link down).
- `background start '{"device":"Polar H10 SIM0001"}'` — the background lease
  itself is the host platform's answer, as always.
- `scan-details` — sees the sim's name, `180D`/`FEEE` service UUIDs and RSSI,
  plus manufacturer data when the profile configures a payload (Linux only).

## Driver host (`peripheral-sim`)

Instead of driving the sim over its control port by hand, join the shared
test driver: the sim connects OUT to the driver server and exposes every
control as `sim-control` scenario commands (`ubm-test-driver/1` host kind
`peripheral-sim`):

```sh
./target/debug/h10-sim --driver ws://127.0.0.1:8795/host
```

The hello advertises the same command table as `help`, so the two can never
drift (a node conformance test decodes the checked-in
`tests/driver-hello.json` fixture through the real `protocol.ts`; CI
regenerates it from the binary). Sim log lines stream back as scenario
events. Combined sequences live in
`examples-shared/driver/server/sequences/`: `h10-sim-drop-link.json`
(android streams from `Polar H10 SIM0001` while peripheral-sim drops the
link: lifecycle loss + reconnect + resumed values) and
`h10-sim-ecg-fault.json` (`reject-next-pmd`, then the DUT's `ecg start`
reports `pmd.request-rejected`).

## Always-on (systemd)

```sh
tool/h10-sim/scripts/install-service.sh [--profile profiles/low-battery-legacy.json] [--token …]
```

Builds the release binary, installs `~/.local/bin/h10-sim`,
`~/.config/h10-sim/profile.json` (kept on re-install) and the
`systemd/h10-sim.service` user unit, then enables and starts it. The script
refuses non-Linux systems, never escalates (run it as the Bluetooth user;
the `bluetooth` group is enough), and stores a token in
`~/.config/h10-sim/env` (mode 600) rather than in the unit. For start-at-boot
without a login session: `loginctl enable-linger $USER`. Verify on the host
with `systemd-analyze verify ~/.config/systemd/user/h10-sim.service`.

## Tests

- `cargo test` — 71 unit tests on macOS/Windows (81 on Linux): encoders (HR
  measurement incl. contact states, PMD ECG frames, control-point responses,
  settings TLV, features, system id), the synthetic ECG waveform plus replay
  files, PMD command handling (start/stop/settings validation, one-shot
  fault, op/type/status errors), profiles (stock + low-battery parsing,
  hex, pair policy), battery drain, RR jitter and BPM curves, the control
  protocol (parsing/validation, token gate over an in-memory duplex,
  bind refusal), the advertisement budget (names plus manufacturer-data
  sizes), the defaulted radio-trait methods, the driver hello/decode shapes,
  the timing model (seeded sampling, fingerprint loading, UNCONFIRMED
  placeholders, checked-in defaults) and the fingerprint comparator
  (synthetic real/sim pairs), and — Linux only, no radio needed — the
  `bluer` GATT application
  declaration (4 services, characteristic counts, control-point
  read/write/indicate flags, read/write flags following each declared
  property and permission with no encryption flags, a read without the
  Readable permission refused with `NotPermitted` before the sim sees it, a
  permission without its property and an unreadable initial value both
  refused, first live reads equal to the declared initial values) and the
  exact `LEAdvertisement1` object (every property pinned).
- `node tests/xcheck/run-xcheck.cjs` (repo root) — emits vectors from the Rust
  encoders via `h10-sim --emit-test-vectors`, compiles the repo's own
  `examples-shared/driver/polar-pmd.ts` and `src/profiles/heart-rate.ts` with
  `tsc`, and decodes every vector through them. No hardware involved.
- `node --test examples-shared/driver/__tests__/peripheral-sim.test.mjs` —
  decodes the checked-in `tests/driver-hello.json` through the real
  `protocol.ts` (regenerate with `h10-sim --emit-driver-hello`; CI diffs it).

## Known fidelity gaps vs a real H10

- Manufacturer-data payload bytes are not pinned to a real capture yet: the
  sim emits company `0x006B` with the profile's payload (empty by default —
  an empty profile payload stays off the air rather than claiming unknown
  bytes), on Linux only. Apple exposes no manufacturer-data peripheral API,
  which the `advertising-started` log states every time. `scan-details` sees
  no `manufacturerCompanyIds` from the default profile.
- `drop-link` on CoreBluetooth stops advertising but cannot force-disconnect
  an active central (no disconnect API); BlueZ disconnects via
  `Device1.Disconnect` (counted in the reply) and releases the GATT app.
- No encryption-gated characteristics: like the real H10, PMD streams without
  a bond; `--pair-policy disabled` is policy state, not a BlueZ pairing
  refusal (a GATT app cannot enforce that — see Pairing and bonding).
- ECG only: no ACC/PPI streams (their PMD types answer `NOT_SUPPORTED`;
  the H10 supports ACC but its PMD frame format needs a documented source
  before implementing).
- Synthetic waveform by default (recorded replay available), synthetic serial
  and system id, 130-sample/s ECG only (no other rates).
- ATT MTU and connection parameters are the platform's answer, not the sim's.

## Fidelity ground truth: capture, compare, time

The simulator models only CONFIRMED H10 behaviour — measured from real
straps with the `h10-capture` driver scenario
(`examples-shared/driver/scenarios/h10-capture.ts`, public UBM API only).
Every behaviour below cites its source: a Polar BLE SDK file, a Bluetooth
SIG spec section, or a capture field. Placeholders the captures have not
confirmed yet are marked `UNCONFIRMED` in
`profiles/timing-default-unconfirmed.json` (pinned by test) and listed at
the end of this section.

### 1. Capture the real straps

On each host, against each strap (10 s scan, 60 s HR stream, 30 ECG frames —
just over a minute per capture):

```sh
node examples-shared/driver/server/cli.mjs serve   # once, on the Mac
node examples-shared/driver/server/cli.mjs capture <host-id> --device "Polar H10 E997042F"
```

Tonight: straps `E997042F` and `E9B93D29`, one capture each from the Samsung
(Expo Android host), the iPhone (Expo iOS host) and the macOS Tauri host —
six files in `fixtures/h10-fingerprints/<hostId>-<serial>-<date>.json` (see
`examples-shared/driver/README.md` for the exact six commands). Then the
simulator fingerprint, run the same scenario against the sim from any radio
host (short windows are fine here):

```sh
./target/debug/h10-sim --driver ws://127.0.0.1:8795/host &
node examples-shared/driver/server/cli.mjs capture <host-id> --device "Polar H10 SIM0001" --hr-ms 10000 --ecg-frames 10 --out /tmp/h10sim
```

The fingerprint (`FINGERPRINT_VERSION = 1`) records the advertisement and
scan response (every field the host exposes, advertising interval over N
seconds with duplicates on, RSSI stats), the full GATT database (services,
characteristics, properties, descriptors, occurrence order, UUIDs), every
readable value (DIS, battery, body sensor location, PMD features and ECG
settings ×3 for a latency distribution), timing distributions (connect,
discovery, HR notification interval and jitter over ≥ 60 s, ECG frame
interval and sample-count consistency, PMD response latency, MTU negotiation
result, time to first HR value), behaviour probes (PMD start without MTU,
repeated start, stop when stopped, an invalid PMD command with its error
code, control-point read while notifying, battery notify support) and host
metadata (host kind, platform, OS version, backend).

### 2. Compare real vs sim

```sh
./target/debug/h10-sim --compare fixtures/h10-fingerprints/<real>.json /tmp/h10sim/<sim>.json
./target/debug/h10-sim --compare <real>.json <sim>.json --tolerance-p50 0.25 --tolerance-ms 50
```

The report is field-by-field JSON with `passed` plus one entry per check:
structural parts (advertisement layout, GATT database, values modulo
configured identity — serial, battery level and local-name id excluded from
equality but still checked readable — behaviour-probe status codes) must be
equal; timing distributions pass when
`|sim − real| ≤ max(min_abs_ms, p50_relative × |real|)`, with the applied
bound printed on every timing check. Tolerances are reported, never hidden.
Exit 0 when every check passes, 1 otherwise. The comparator is unit-tested
with synthetic fingerprints (`src/compare.rs`).

### 3. Timing model

`src/timing.rs` loads the measured distributions from a fingerprint (or a
saved timing profile) and samples semi-random delays from them: HR
notification intervals, PMD response latency, ECG frame jitter. Sampling is
seeded (SplitMix64) — the same capture plus `--timing-seed` replays the
same run exactly:

```sh
./target/debug/h10-sim --timing-profile fixtures/h10-fingerprints/<real>.json --timing-seed 7
h10-sim --emit-timing-defaults   # regenerate profiles/timing-default-unconfirmed.json
```

Until the captures exist the sim runs on the documented defaults and its
over-the-air behaviour is unchanged (zero-spread placeholders sample
deterministically; connect/discovery/MTU latencies stay the central's own
answers and are never synthesized).

### Behaviour sources and UNCONFIRMED list

| Behaviour | Source |
| --- | --- |
| HR flags `0x16`, RR in 1/1024 s, chest location `1` | SIG HRS 1.0 §3.3–§3.4 (`src/gatt_spec.rs`) |
| Battery uint8 percent; DIS strings UTF-8; System ID 8 bytes | SIG BAS 1.1 §3.2; SIG DIS 1.1 (`src/gatt_spec.rs`) |
| PMD response `[0xF0, op, type, status, more, params…]`, ECG frames `[0x00, tsNs u64 LE, 0x00, s24 LE µV]`, 130 Hz / 14 bit, feature bitmap ECG = 0x01, settings TLV, status codes | Polar BLE SDK `BlePMDClient` / `PmdControlPointResponse` / `PmdDataFrame` / `PmdSetting` / `PmdMeasurementType` (`src/gatt_spec.rs`, `examples-shared/driver/polar-pmd.ts`) |
| Advertisement: Flags + 16-bit UUID list in AD, name in scan response | BlueZ 5.72 `src/advertising.c` layout (`src/advertisement.rs`) |
| HR ~1 Hz cadence, PMD response latency, ECG frame jitter, advertising interval | capture `timings.*` — **all four UNCONFIRMED** (see below) |

UNCONFIRMED until a capture confirms them (all four in
`profiles/timing-default-unconfirmed.json`):
`hr_interval` (~1000 ms), `pmd_response` (0 ms — local answer, no documented
value), `ecg_frame_jitter` (0 ms around 2 frames/s × 65 samples),
`advertising_interval` (100 ms placeholder; the interval stays the platform
radio's answer either way).
