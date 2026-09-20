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
| Linux | `bluer` directly (BlueZ GATT server + `LEAdvertisement1`); opt-in `--linux-advertising mgmt-legacy` adds the advertisement on the kernel MGMT socket (`libc`) | 0.17.4 |
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

`--linux-advertising mgmt-legacy` (Linux, below) writes the same bytes itself
(`src/mgmt.rs`, golden-byte tests): advertisement data `02 01 06 05 03 0D 18
EE FE` (+ manufacturer data when staged), scan response `12 09 "Polar H10
SIM0001"`. The Flags AD is written by the sim with the real H10's `0x06`
(LE General Discoverable, BR/EDR not supported), so the instance is added
connectable with no kernel-managed flag bits; the kernel would otherwise
write its own Flags byte, which follows the adapter's BR/EDR setting.

## Simulated GATT surface

Advertised name `Polar H10 SIM<4 hex>` (default `Polar H10 SIM0001`,
`--name` overrides), advertising Heart Rate (`180D`) and Polar (`FEEE`),
plus Polar manufacturer data (company `0x006B`) with the profile's payload
on Linux — Apple exposes no manufacturer-data peripheral API (an OS
limitation), so the bytes stay off the air there; the `advertising-started`
log states which happened every time. See fidelity gaps for the payload
variance.

| Service | Characteristics | Properties |
| --- | --- | --- |
| Heart Rate `180D` | `2A37` measurement: notify ~1 Hz, flags `0x10` (uint8 bpm, RR present, contact not supported — like the strap's 120 captured packets); contact bits only when the profile declares `contact_supported` | notify |
| | `2A38` body sensor location: chest (`1`) | read |
| Device Information `180A` | `2A29` manufacturer `Polar Electro Oy`, `2A24` model `H10`, `2A25` serial, `2A27` hardware, `2A26` firmware, `2A28` software, `2A23` system id (hardware before firmware, like the strap; every string NUL-terminated, like the strap) | read |
| Battery `180F` | `2A19` level (default 90%, the captured charge state) | read, notify (60 s) |
| Polar vendor `6217FF4B-…` | `6217FF4C-…` readable (value UNCONFIRMED, served empty) | read |
| | `6217FF4D-…`: write-command, indications (no behaviour model: writes are refused loudly, nothing is ever indicated) | write-without-response, indicate |
| Polar PMD `FB005C80-…` | `FB005C81` control point: read returns features (ECG + ACC, the strap's exact 17 bytes); write `0x01` get-settings / `0x02` start / `0x03` stop, each answered with an indicate `[0xF0, op, type, status, more, params…]` | read, write, indicate |
| | `FB005C82` data: ECG frames, 73 samples at 130 Hz (~561.6 ms cadence, `[0x00, timestampNs u64 LE, 0x00, samples…]`, signed 24-bit LE µV, the real strap recording by default) | notify |
| Polar `FEEE` | `FB005C51-…` (write, write-command, notify), `FB005C52-…` (notify), `FB005C53-…` (write, write-command): no behaviour model, writes refused loudly, nothing ever notified | mixed |

Services, their order and the characteristic counts/properties match the
captured strap fingerprints in `fixtures/h10-fingerprints/` exactly
(`180D`, `180A`, `180F`, `6217FF4B`, PMD, `FEEE`; seven CCCDs). Indication
confirmations keep the subscription session up: BlueZ reports each
confirmation on the notify file descriptor, and only a closed descriptor ends
the session (logged as `indication-confirmed` vs `unsubscribed`).

No PnP ID (`2A50`), like the real H10 — `device-info read` reports that read
as its own failed outcome. Start commands must request 130 Hz / 14 bit;
anything else is refused with the SDK status codes (`ERROR_INVALID_SAMPLE_RATE`
0x08, `ERROR_INVALID_RESOLUTION` 0x07). A repeated start and a stop while
idle answer `ERROR_ALREADY_IN_STATE` (0x06) without changing the stream, like
the strap. Valid Polar types the H10 cannot stream here (PPG/PPI, and ACC —
the features bitmap advertises it but its frame format is UNCONFIRMED) answer
`ERROR_NOT_SUPPORTED` (0x03).

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
cargo test         # 122 unit tests on macOS/Windows, 134 on Linux (see Tests below)
node tests/xcheck/run-xcheck.cjs   # run from the repo root; see Tests below
cargo clippy --all-targets -- -D warnings   # must stay warning-free
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
  [--bpm 72] [--battery 90] [--pair-policy just-works] [--ecg-file ecg.txt]
  [--hr-replay fixtures/h10-raw/tauri-E9B93D29-2026-09-19-raw.json]
  [--timing-profile fixtures/h10-fingerprints/<real>.json] [--timing-seed 7]
  [--control-bind 127.0.0.1] [--control-port 17935] [--control-token-file token.txt]
  [--driver ws://127.0.0.1:8795/host] [--linux-advertising bluez|mgmt-legacy]
```

Later flags win: `--profile` applies first, then `--name`/`--bpm`/`--battery`.

- **macOS: launch from Terminal.app** (or whichever terminal owns the window),
  because Bluetooth permission follows the launching process. The sim waits up
  to 30 s for a powered adapter and fails loudly otherwise.
- **Linux:** see Linux requirements below.

## Linux requirements

- `bluetoothd` running (BlueZ 5.72 and 5.85 verified; advertising on current
  kernels needs `--linux-advertising mgmt-legacy`, see below), adapter powered; build needs
  `libdbus-1-dev` (`sudo apt install libdbus-1-dev`).
- Known identity: the default profile advertises `Polar H10 SIM0001` with
  serial `SIM000001`, firmware `5.0.0` / hardware `00760690.03` / software
  `4.2.0` (the strap's revisions), battery 90% and the strap's System ID
  (`3db9e9feff1a9ea0`). The radio address is the controller's own public address, not
  the sim's to choose — read it with `bluetoothctl show` (controller
  `90:DE:80:3B:69:78` on the reference host, `DC:56:7B:D9:E8:A4` on
  lx5090wifi) and document it beside the
  profile. Driver scenarios target the sim by name (`device: "Polar H10
  SIM0001"`), so the address never enters a command; use the address only to
  confirm over the air (e.g. in `btmon` or `bluetoothctl devices`) that the
  peer you found is this host.
- D-Bus access to `org.bluez`: run as root or as a user in the `bluetooth`
  group (check `groups`; `sudo usermod -aG bluetooth $USER` then log back in).
  An access denial surfaces immediately as a D-Bus `AccessDenied` error, not
  as a registration failure.
- BlueZ's own Device Information service must be disabled: bluetoothd core
  exposes a second `180A` (PnP-ID only) next to every peripheral GATT
  application (`src/gatt-database.c` `populate_devinfo_service`, gated on the
  Device ID source — not the `deviceinfo` plugin, which is only the client
  side), so every `180A` read resolves `gatt.ambiguous-path` over the air.
  There is no per-application opt-out, so the reference host sets
  `DeviceID = false` under `[General]` in `/etc/bluetooth/main.conf`, then
  `sudo systemctl restart bluetooth` and restarts the sim (its GATT
  registration is lost with the daemon). Two central-view deltas versus a
  real strap remain, neither breaking any scenario: host services whose
  UUIDs match BlueZ's MIDI profile (`03B80E5A-…`, `7772E5DB-…`, see
  `profiles/midi/libmidi.h`) plus unattributed host services
  (`d0611e78-…`, `9fa480e0-…`), and ATT attribute order, which varies per sim
  registration because bluer's `GetManagedObjects` reply serializes from a
  `HashMap` (`dbus-crossroads` 0.5.3 `stdimpl.rs` `PathPropMap`) — the
  declared order is the strap's, but BlueZ numbers handles in enumeration
  order.
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
  (`ActiveInstances=… SupportedInstances=…`) and the kernel release.

### Linux advertising: `bluez` (default) and `mgmt-legacy` (opt-in)

**Root cause of `Failed to register advertisement`.** bluetoothd adds an
`LEAdvertisement1` object to the kernel with `MGMT_OP_ADD_EXT_ADV_PARAMS`
(0x0054) then `MGMT_OP_ADD_EXT_ADV_DATA` (0x0055), and sizes the 0x0055
parameters with `sizeof(struct mgmt_cp_add_advertising)` (11 bytes) where
`struct mgmt_cp_add_ext_adv_data` is 3: every request carries 8 extra zero
bytes (BlueZ `src/advertising.c`, 5.72 and master). Kernels that check the
parameter length exactly answer `Invalid Parameters (0x0d)`, so **every**
D-Bus advertisement fails, `bluetoothctl advertise on` included, whatever its
properties. Proven with `btmon` on two hosts: Ubuntu 24.04 / kernel
6.8.0-139 / BlueZ 5.72, and Ubuntu 26.04 / kernel 7.0.0-30 / BlueZ 5.85. The
journal shows `add_client_complete() Failed to add advertisement: Invalid
Parameters (0x0d)`. GATT application registration is unaffected, and the
older, correctly sized `MGMT_OP_ADD_ADVERTISING` (0x003E) works on the same
controllers.

**`--linux-advertising bluez`** (default, no privilege): today's path. On the
failure above the sim names the mismatch, the journal line that confirms it
and the `mgmt-legacy` workaround, then exits with status **78** without
retrying: each failed registration leaks a kernel advertising instance
(`btmgmt advinfo`; `sudo systemctl restart bluetooth` frees them). The units
set `RestartPreventExitStatus=78` so systemd does not retry either.

**`--linux-advertising mgmt-legacy`** (explicit opt-in): the GATT application
stays on bluetoothd (via `bluer`, as before); the advertisement is added by
the sim on a raw MGMT socket (`AF_BLUETOOTH`/`BTPROTO_HCI` bound to
`HCI_CHANNEL_CONTROL`, `src/mgmt_socket.rs`) with `MGMT_OP_ADD_EXT_ADV_PARAMS`
(0x0054) plus `MGMT_OP_ADD_EXT_ADV_DATA` (0x0055): the exact H10 payload
above, connectable, no duration or timeout, and the strap's ~1 s advertising
interval (min = max = 1600 in 0.625 ms HCI units, from the strap captures'
`advertisementIntervalMs`: p50 1004 ms on Android, 1042 ms on Tauri — the
legacy command cannot express an interval at all). Both commands are exactly
sized (`src/mgmt.rs`, golden-byte tests against
`include/net/bluetooth/mgmt.h`); only when the kernel refuses 0x0054 itself
does the sim fall back to the correctly sized `MGMT_OP_ADD_ADVERTISING`
(0x003E), reporting the fallback on stderr, in the returned start outcome
and in the `advertising-started` detail (`"method":
"legacy-0x003e-fallback") — never silently.

- **Privilege.** The kernel only trusts a control socket whose process holds
  `CAP_NET_ADMIN`. The sim checks `CapEff` first and, without it, exits 78
  naming the command; it never runs `sudo` or asks for escalation. Grant it
  to the binary yourself:

  ```sh
  sudo setcap cap_net_admin+ep target/debug/h10-sim   # after every cargo build
  getcap target/debug/h10-sim                         # cap_net_admin=ep
  ./target/debug/h10-sim --linux-advertising mgmt-legacy --profile profiles/stock-h10.json
  ```

- **Blast radius.** Anyone who can run that file gets `CAP_NET_ADMIN` in it:
  the binary can then issue any Bluetooth management command on every
  adapter (power, discoverable/connectable, pairing and privacy settings,
  advertising of other programs) and administer other network interfaces
  (routes, firewall, addresses). Grant it only on a dedicated test host, to a
  binary only you can write. Remove it with `sudo setcap -r
  target/debug/h10-sim` (a rebuild also replaces the file and drops it). A
  failed start leaves no setting behind: the sim changes nothing but its own
  advertising instance.
- **Instance choice.** `MGMT_OP_READ_ADV_FEATURES` lists the instances in use;
  the kernel lists only instances numbered at most its instance count, so the
  sim takes the highest number it can prove free (a gap below the listed
  count, else count + 1), keeping clear of bluetoothd's lowest-first ids. The
  `advertising-backend` and `advertising-started` log lines name the listed
  instances and the one taken.
- **Cleanup.** The instance is removed with `MGMT_OP_REMOVE_ADVERTISING` on
  `set-advertising off`, `drop-link`, SIGINT, SIGTERM (systemd stop), on drop,
  and from a panic hook when the simulator loop panics. A run killed with
  SIGKILL cannot clean up; it leaves a record
  (`$XDG_RUNTIME_DIR/h10-sim-mgmt-hci0.instance`, else the temp dir) with the
  boot id and instance, and the next start in the same boot removes that
  instance (`"stale":{"removedInstance":N}` in `advertising-backend`). A
  removal by anyone else is read from the kernel's `Advertising Removed`
  event. Check with `sudo btmgmt advinfo`; remove by hand with `sudo btmgmt
  rm-adv <instance>`.
- **Residual risk.** Another MGMT client (bluetoothd, `btmgmt`) that adds an
  instance with the sim's number replaces it without an event; do not run a
  second advertiser on the same controller.

### Adapter alias while running (Linux, both advertising paths)

BlueZ serves its own Generic Access service beside the sim's GATT application,
and its Device Name characteristic follows `btd_adapter_get_name`, which
prefers the stored adapter alias over the system name (`src/adapter.c`).
So a central that connects reads the host name (`lx5090`) — not the strap
name — unless the sim changes it. While advertising, the sim therefore sets
the adapter alias (`org.bluez.Adapter1.Alias` via D-Bus, through `bluer`) to
the advertised name, and restores the previous alias afterwards. MGMT Set
Local Name (0x000F) would not do it: it changes only the system name, which
the stored alias shadows.

- **What changes.** Exactly one D-Bus property on the advertising adapter,
  from the previous alias to the advertised name (e.g. `lx5090` →
  `Polar H10 SIM0001`), plus a record file
  (`$XDG_RUNTIME_DIR/h10-sim-alias-hci0.instance`, else the temp dir) holding
  the boot id and the previous alias. The claim and every restore are printed
  on stderr and the claim rides in the `advertising-started` detail as
  `"adapterAlias": {"previous": …, "current": …}`.
- **Privilege.** None beyond what the radio already needs: the same D-Bus
  access that registers the GATT application. The sim never escalates; a
  D-Bus denial fails startup loudly.
- **Blast radius.** While the sim runs, anything reading the adapter sees the
  strap name: `bluetoothctl show` (`Alias:`), GAP Device Name reads from
  connected centrals, other Bluetooth apps on the host. Paired-device entries
  are untouched, and nothing persists once restored.
- **Restore.** The previous alias is restored by `stop-advertising`
  (`set-advertising off`, and the SIGINT/SIGTERM shutdown path, which stops
  advertising before exiting),
  from a main-thread panic hook (which runs `bluetoothctl system-alias
  <previous>` — `reset-alias` when the previous alias was empty — and keeps
  the record when that fails), and by stale-record adoption on the next start
  (a leftover sim-name alias with a same-boot record restores the recorded
  alias at the next stop; a record from another boot or controller is
  discarded loudly). Verify with `bluetoothctl show` (`Alias:`) and, over the
  air, by reading GAP Device Name after connecting.
- **Residual risk.** A run killed with SIGKILL between the alias change and
  the next start leaves the strap name as the adapter alias; the next start
  adopts the record and the next stop restores it. If the record is deleted
  by hand, the next start takes the leftover name as the previous alias and
  restores that — check `bluetoothctl show` and fix by hand with
  `bluetoothctl system-alias <name>`.

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
| `{"cmd":"set-contact","detected":false}` | Sensor-contact lost/detected — recorded always, but changes the HR flags (`0x04`/`0x06`) only when the profile declares `contact_supported` (the stock strap profile does not) |
| `{"cmd":"pair-policy","policy":"disabled"}` | Pairing policy `just-works`/`disabled` |
| `{"cmd":"load-profile","path":"…"}` | Load a profile file live (re-advertises when advertising) |
| `{"cmd":"set-advertising","on":false}` | Stop/start advertising |
| `{"cmd":"drop-link"}` | [adversarial] Halt ECG and disconnect the simulator's tracked GATT clients plus any `--drop-link-allow` extras, reporting `dropped`/`skipped` per address in `state` (no targets reports the note `no simulator clients` and disconnects nothing); advertising and the GATT database stay up so centrals see a lifecycle loss with no Service Changed and can reconnect at once (BlueZ `Device1.Disconnect`; on CoreBluetooth a connected central stays connected — no disconnect API) |
| `{"cmd":"set-silent","on":true}` | [adversarial] Stop notifying while keeping the link up |
| `{"cmd":"reject-next-pmd","status":3}` | [adversarial] Fail the next PMD command with a status code, then clear |
| `{"cmd":"clear-pmd-fault"}` | [adversarial] Disarm without firing |
| `{"cmd":"delay-responses","ms":250}` | [adversarial] Add `ms` of extra PMD response latency on top of any measured latency (`0` clears) |
| `{"cmd":"flap-link"}` | [adversarial] Drop the simulator's client links and bounce advertising so centrals run a rapid disconnect/reconnect cycle |
| `{"cmd":"interrupt-next-subscribe"}` | [adversarial] Tear down the next notify/indicate subscription as soon as it is set up |
| `{"cmd":"stale-callback"}` | [adversarial] Re-notify the last PMD response out of sequence (fails loudly when no PMD response has gone out yet) |
| `{"cmd":"constrain-delivery","keepEvery":4}` | [adversarial] Deliver every `keepEvery`-th ECG frame only (`1` disables) |
| `{"cmd":"set-rates","hrHz":2.0,"ecgFramesPerSec":1.78,"ecgFrameSamples":73}` | Stream rates (`hrHz` 0.1–10, `ecgFramesPerSec` 0.5–10, samples 1–167 so a frame fits MTU 512; strap defaults 1 Hz / 73 samples / 130/73 fps) |
| `{"cmd":"run-record"}` | Report this run's seed/profile, `--mode` and injected fault sequence with timestamps (telemetry, available in every mode) |
| `{"cmd":"get-state"}` | Current state snapshot |
| `{"cmd":"help"}` | Command list (generated from the same table the driver hello uses) |

Unknown commands and out-of-range values get `{"ok":false,"error":"…"}`.

### Run mode

`--mode faithful|adversarial` (default `faithful`) selects the run posture
once at startup; profiles cannot change it. `faithful` reproduces the
captured H10 behaviour and injects nothing: every command marked
`[adversarial]` above is refused with `{"ok":false,"error":"…is an
adversarial fault command: refused in faithful mode…"}`. `adversarial`
additionally allows those commands to inject labelled faults. Configuration
commands, `run-record`, `get-state` and `help` stay available in every mode.
The acceptance suite's link-loss scenario drives `drop-link`, so it needs
`--mode adversarial`.

`run-record` reports `{"ok":true,"state":{"mode":"faithful"|"adversarial",
"seed":<u64>,"profile":"<path or \"<builtin stock-h10>\">","name":"<device
name>","startedAt":"<RFC 3339>","faults":[{"ts":"<RFC 3339>","fault":"<command
name>","detail":{…}},…]}}` — one entry per fired adversarial command, in
order, each with its own detail (e.g. `drop-link` records `dropped` and
`targets`).

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
clears the curve. Contact simulation needs `"contact_supported": true` in
the profile's `heart_rate` section — without it the flags stay at the
strap's `0x10`, like the real H10 ignoring contact. `--ecg-file` (or
`"ecg_source": {"file": …}`) replays a recorded trace: text, one integer µV
per line at 130 Hz, cycling forever; a missing file or a bad line fails
startup with its line number. The stock ECG source is `"recorded"`: the real
strap recording compiled in from
`fixtures/h10-raw/ecg-E9B93D29-2026-09-19-130hz.txt` (3285 samples, ~25 s,
cycling forever), so the default stream carries real QRS complexes from any
directory. `--hr-replay <raw-capture.json>` (or `"hr_source": {"file": …}`)
replays the recorded `2A37` packets verbatim instead of synthesizing one RR
interval from the bpm. The H10 `2A37` flags carry no energy-expended bit, so
energy expended is intentionally not simulated.

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
- `scan-details` — sees the sim's name, `180D`/`FEEE` service UUIDs, Polar
  manufacturer data (company `0x006B`, Linux only) and RSSI.

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
  [--linux-advertising bluez|mgmt-legacy] [--system-unit]
```

Builds the release binary, installs `~/.local/bin/h10-sim`,
`~/.config/h10-sim/profile.json` (kept on re-install) and the
`systemd/h10-sim.service` user unit, then enables and starts it. The script
refuses non-Linux systems, never escalates (run it as the Bluetooth user;
the `bluetooth` group is enough), and stores a token in
`~/.config/h10-sim/env` (mode 600) rather than in the unit. For start-at-boot
without a login session: `loginctl enable-linger $USER`. Verify on the host
with `systemd-analyze verify ~/.config/systemd/user/h10-sim.service`. Both
units carry `RestartPreventExitStatus=78` (advertising unavailable, see
Linux advertising).

`--linux-advertising mgmt-legacy` needs `CAP_NET_ADMIN`, and a user unit
cannot grant capabilities (`AmbientCapabilities=` fails with
`218/CAPABILITIES` under the user manager). Two ways, both granted by the
owner with sudo, never by the script:

- user unit (default): the mode goes to `~/.config/h10-sim/env` as
  `H10SIM_LINUX_ADVERTISING=mgmt-legacy`; the script prints `sudo setcap
  cap_net_admin+ep ~/.local/bin/h10-sim` and does not start the unit until
  `getcap` shows it (each install replaces the binary and drops it);
- `--system-unit`: renders `systemd/h10-sim-mgmt-legacy.service.in` into
  `~/.config/h10-sim/h10-sim-mgmt-legacy.service` — `User=` you,
  `AmbientCapabilities=CAP_NET_ADMIN`, `CapabilityBoundingSet=CAP_NET_ADMIN`,
  `NoNewPrivileges=yes`, so the capability exists only inside that service and
  no file capability sits on disk — verifies it, and prints the `sudo
  install` / `systemctl enable --now` commands and how to remove it.

## Tests

- `cargo test` — 112 unit tests on macOS/Windows (124 on Linux, which adds the `bluer`
  backend tests): encoders (HR measurement with the strap's `0x10` default
  plus explicit contact states, DIS NUL termination, PMD ECG frames,
  control-point responses, settings TLV, features incl. the strap's 17
  bytes, system id, 73-sample frame geometry), the synthetic ECG waveform,
  the compiled-in strap recording plus replay files, the HR replay loader
  against the committed raw capture, PMD command handling (start/stop/settings
  validation, repeated-start/idle-stop `ALREADY_IN_STATE`, one-shot
  fault, op/type/status errors), profiles (stock strap identity +
  low-battery parsing, hex, pair policy, contact/HR-replay defaults),
  battery drain, RR jitter and BPM curves, the control
  protocol (parsing/validation, token gate over an in-memory duplex,
  bind refusal), the advertisement budget (names plus manufacturer-data
  sizes), the defaulted radio-trait methods, the H10 service layout against
  the fingerprints (service order, DIS order, vendor/FEEE properties, seven
  CCCDs), the driver hello/decode shapes,
  the timing model (seeded sampling, fingerprint loading, UNCONFIRMED
  placeholders, checked-in unconfirmed + measured profiles, ECG jitter
  recentering), the fingerprint comparator
  (synthetic real/sim pairs, set-based GATT, host-side availability/delivery
  normalization), the fidelity test comparing each committed real
  fingerprint against the in-process sim fingerprint, the `mgmt-legacy` MGMT packets against the
  kernel's `include/net/bluetooth/mgmt.h` layouts (golden `Add Advertising`
  packet, advertisement data, name scan response, connectable flag with no
  kernel-managed Flags bits, Command Complete / Status / Advertising Removed
  parsing, `Read Advertising Features` replies, instance choice), the Linux
  advertising mode, `CapEff` parsing, the setcap message, the BlueZ mismatch
  diagnosis and the stale-instance record and cleanup decision — all pure,
  so they run on macOS too — and — Linux only, no radio needed — the
  `bluer` GATT application
  declaration (6 services, characteristic counts, control-point
  read/write/indicate flags, write-without-response flags, read/write flags
  following each declared property and permission with no encryption flags,
  a read without the Readable permission refused with `NotPermitted` before
  the sim sees it, a permission without its property and an unreadable
  initial value both refused, indication-confirmation drains keeping the
  session while a closed fd ends it, first live reads equal to the declared
  initial values) and the exact `LEAdvertisement1` object (every property
  pinned).
- `node tests/xcheck/run-xcheck.cjs` (repo root) — emits vectors from the Rust
  encoders via `h10-sim --emit-test-vectors`, compiles the repo's own
  `examples-shared/driver/polar-pmd.ts` and `src/profiles/heart-rate.ts` with
  `tsc`, and decodes every vector through them. No hardware involved.
- `node --test examples-shared/driver/__tests__/peripheral-sim.test.mjs` —
  decodes the checked-in `tests/driver-hello.json` through the real
  `protocol.ts` (regenerate with `h10-sim --emit-driver-hello`; CI diffs it).

## Known fidelity gaps vs a real H10

- Manufacturer-data payload bytes vary on the real strap: the three captures
  disagree (`3f155252` Tauri, `371b6968` Android, `3b00005b` iOS — possibly a
  per-boot token), so no fixed default can be byte-exact. The stock profile
  replays the Tauri-observed bytes with company `0x006B` (Linux only — Apple
  exposes no manufacturer-data peripheral API, which the
  `advertising-started` log states every time); only the company id is
  pinned, by the comparator and the fidelity test.
- PMD response latency is bimodal in the captures: the early-connection
  `pmdResponseMs` distribution sits near ~1 s on all three hosts, while the
  fifteen later behaviour-probe round trips (all five probes × three hosts)
  sit near ~100 ms. The timing model follows the `pmdResponseMs`
  distribution it is built from; the steady-state fast path is documented
  here, not modelled.
- `drop-link` drops the link, not the peripheral: advertising and the GATT
  database stay up, so centrals see a lifecycle loss with no Service Changed
  and reconnect at once. On CoreBluetooth it cannot force-disconnect an
  active central (no disconnect API); BlueZ disconnects via
  `Device1.Disconnect` (counted in the reply).
- No encryption-gated characteristics: like the real H10, PMD streams without
  a bond; `--pair-policy disabled` is policy state, not a BlueZ pairing
  refusal (a GATT app cannot enforce that — see Pairing and bonding).
- ECG only: no ACC/PPI streams (their PMD types answer `NOT_SUPPORTED`;
  the features bitmap advertises ACC like the strap, but its PMD frame format
  needs a documented source before implementing). The vendor `6217ff4c` value
  and the FEEE characteristics' payloads are likewise UNCONFIRMED (empty /
  refused loudly, never guessed).
- Recorded ECG and HR replay cycle one strap session (~25 s ECG, 120 HR
  packets); the synthetic serial stays (`SIM000001`), and 130 Hz is the only
  rate (no other sample rates).
- ATT MTU, connection parameters and the advertising interval are the
  platform's answer, not the sim's. The Android capture additionally shows
  platform-injected GAP/GATT services (`1800`/`1801`) that CoreBluetooth and
  btleplug hide; the fidelity test strips them as central-platform surface.
  Characteristic `availability` and battery `effectiveDelivery` likewise
  differ by central backend, so the comparator judges the SIG flags and the
  settled subscription facts only.

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
structural parts (advertisement layout incl. manufacturer company id, GATT
database as a set — ATT order is a backend artifact, values modulo
configured identity — serial, battery level and local-name id excluded from
equality but still checked readable — behaviour-probe status codes) must be
equal; timing distributions pass when
`|sim − real| ≤ max(min_abs_ms, p50_relative × |real|)`, with the applied
bound printed on every timing check. Host-side observations are reported,
never judged: characteristic `availability`, battery `effectiveDelivery` and
MTU `effective`. Tolerances are reported, never hidden. Exit 0 only when every
check passes AND coverage is complete, 1 otherwise (FX4); --allow-incomplete
accepts passed-alone explicitly and never applies to --qualify-ota. The
comparator is unit-tested with synthetic
fingerprints (`src/compare.rs`). Without going over the air,
`--emit-sim-fingerprint` prints the in-process sim fingerprint for the same
comparison:

```sh
./target/debug/h10-sim --emit-sim-fingerprint > /tmp/sim-fp.json
./target/debug/h10-sim --compare fixtures/h10-fingerprints/<real>.json /tmp/sim-fp.json
```

The structural fidelity tests (`src/fidelity.rs`, default tolerances 0.25 /
50 ms; the Android capture is normalized by stripping its
platform-injected `1800` / `1801` services first) assert no checked field
disagreed (passed) and are partial by construction — point distributions
and omitted central-side timings always leave Incomplete corners.
Over-the-air equivalence is a real capture vs a real simulator run:
`h10-sim --qualify-ota <real>.json <sim-run>.json`, which requires passed
AND complete.

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

The sim runs on the measured strap profile by default
(`profiles/timing-h10-measured.json`, fitted from the Tauri capture, pinned
by test against the loader output).
`profiles/timing-default-unconfirmed.json` keeps the original zero-spread
placeholders for explicit opt-in; connect/discovery/MTU latencies stay the
central's own answers and are never synthesized.

### Behaviour sources and UNCONFIRMED list

| Behaviour | Source |
| --- | --- |
| HR flags `0x10` (RR present, contact not supported), RR in 1/1024 s, chest location `1` | SIG HRS 1.0 §3.3–§3.4 + the 120 raw packets in `fixtures/h10-raw/` (`src/gatt_spec.rs`) |
| Battery uint8 percent; DIS strings UTF-8 with trailing NUL; System ID 8 bytes | SIG BAS 1.1 §3.2; SIG DIS 1.1 + `fixtures/h10-fingerprints/` (`src/gatt_spec.rs`) |
| PMD response `[0xF0, op, type, status, more, params…]`, ECG frames `[0x00, tsNs u64 LE, 0x00, s24 LE µV]`, 130 Hz / 14 bit, 73-sample frames, settings TLV, status codes | Polar BLE SDK `BlePMDClient` / `PmdControlPointResponse` / `PmdDataFrame` / `PmdSetting` / `PmdMeasurementType` (`src/gatt_spec.rs`, `examples-shared/driver/polar-pmd.ts`) + `fixtures/h10-fingerprints/` |
| GATT database (services, counts, properties, DIS hardware-before-firmware order, seven CCCDs), PMD feature bytes (`0f0500…`, 17 bytes, ECG + ACC), `ALREADY_IN_STATE` on repeated start / idle stop, indication confirmations keeping the session | h10-capture fingerprints `fixtures/h10-fingerprints/` (all three capture hosts agree) |
| Advertisement: Flags + 16-bit UUID list in AD, name in scan response, Polar company `0x006B` | BlueZ 5.72 `src/advertising.c` layout (`src/advertisement.rs`) + `fixtures/h10-fingerprints/` |
| HR interval (p50 993 ms), PMD response (p50 994 ms), ECG frame jitter (spread 0.009 ms around the 73/130 s cadence), advertising interval (p50 1042 ms) | Tauri capture `timings.*` / `advertisement.*` — **all four CONFIRMED** in `profiles/timing-h10-measured.json` |

Still UNCONFIRMED (placeholders in `profiles/timing-default-unconfirmed.json`
for explicit opt-in; live defaults are the measured profile above): nothing
timing-related remains — the open gaps are the vendor `6217ff4c` value, the
FEEE payloads and ACC streaming (see fidelity gaps), which need a documented
source before implementing.
