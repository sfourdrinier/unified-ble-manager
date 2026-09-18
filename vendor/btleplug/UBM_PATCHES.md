# UBM patches to btleplug 0.12.0

This directory is btleplug 0.12.0 from crates.io (checksum
`52c3264dbe2c8e29381e4e95aa2d2783ad0b9192b511240f3755b7e5e3cee87e`).
It is used through `[patch.crates-io]` in the root `Cargo.toml`,
`native/tauri/Cargo.toml` and `example-tauri/src-tauri/Cargo.toml`.
PR210 decision 10 allows patching btleplug only where parity needs native
objects that btleplug owns. Every change against upstream is listed below.
The upstream licence (`LICENSE.md`) applies to the whole copy.

## What was not vendored

- `examples/`, `tests/`, `test-peripheral/`, `scripts/` and `docs/`. The
  `[[example]]` and `[[test]]` sections that pointed at them were removed
  from `Cargo.toml`.
- `src/droidplug/java/`, the Android Java sources and Gradle wrapper. They
  are built separately for Android, and no host that uses this copy is
  Android.

## Manifest

`Cargo.toml` adds `build = "build.rs"` and `links = "btleplug_ubm"`, and
drops upstream's `[dev-dependencies]` (they served the unvendored examples
and integration tests; without them `cargo test -p btleplug` runs this
copy's unit tests, including the patch tests below).
`build.rs` publishes the applied patch list as `cargo:patches=...`, which
direct dependents read as `DEP_BTLEPLUG_UBM_PATCHES`.
`crates/ubm-desktop/build.rs` turns the list into `cfg(btleplug_ubm_*)`
flags for patches 1-5 and republishes it for diagnostics
(`btleplug_backend::vendored_btleplug_patches()`). Patches 6-18 are
required: the production radio does not build against a btleplug without
them (the build fails naming the missing patches), because without them
same-UUID attributes collapse, adapter loss states are unreadable, scan
duplicates ignore the caller, WinRT discovery reads a stale cache,
broadcast loss is silent, WinRT scans actively, BlueZ discovery ignores
the caller's name prefix, 16-slot broadcasts lose events below the
legacy backends' queues, a CoreBluetooth notification silently becomes a
read result, platform failures lose their identity, WinRT scans without the legacy OS
service filter, sightings are lost or carry another advertisement's
data, and a BlueZ name-only change is invisible.

The crate-level usage example in `src/lib.rs` (a `no_run` doctest) cycles
fixed colours instead of using `rand`, so it compiles without upstream's
dev-dependencies.

The Android backend (`src/droidplug`) is not built by any host of this copy
and has not been brought up to patches 6 and 10 (its literals lack the
instance fields); it would need that work before an Android build could
use this copy.

## Patch 1: `corebluetooth-write-length`

**Problem.** Upstream `corebluetooth::Peripheral::mtu()` stays at the
initial `DEFAULT_MTU_SIZE` (23) forever. btleplug never exposes
`-[CBPeripheral maximumWriteValueLengthForType:]`. The desktop core caps
writes at the measured limit, so every macOS write was capped at 20 bytes,
and the legacy CoreBluetooth backend's `gatt:maximum-write-length` and its
core-emulated long write had no replacement.

**Change.**

- `src/corebluetooth/internal.rs`:
  - New message `CoreBluetoothMessage::GetWriteLengths`, handled on the
    CoreBluetooth thread that owns the `CBPeripheral`.
  - New reply `CoreBluetoothReply::WriteLengths { with_response,
    without_response }`, read from `maximumWriteValueLengthForType:` for
    both write types.
- `src/corebluetooth/peripheral.rs`:
  - New inherent method `Peripheral::maximum_write_value_lengths()`, which
    returns `(with_response, without_response)` as `u16` and saturates
    larger values.
  - The same method also stores the measured ATT MTU (the without-response
    length + 3), so upstream's `mtu()` stops reporting 23.
  - An unexpected reply is an error, not a panic.
- `build.rs` lists `corebluetooth-write-length`.

**Tests.**

- `crates/ubm-desktop` `btleplug_backend::tests::the_vendored_btleplug_patch_set_is_detected`
  fails if a build in this workspace does not link the patched copy.
- `capabilities::tests::per_os_verdicts_are_well_formed_and_reported` and
  `tests/legacy_parity.rs` require macOS `gatt:maximum-write-length` to be
  registered, which happens only when the patch is detected.
- `tests/parity_ops.rs`
  `maximum_write_length_is_per_mode_and_matches_what_write_enforces` covers
  the per-mode limit path over the scriptable radio.
- Physical check, which needs a Bluetooth-authorized macOS terminal and a
  connected peripheral: `maximum_write_length(peer, lease, path, true/false)`
  returns CoreBluetooth's values instead of 20. This has not been run yet.

## Refreshing

When btleplug is upgraded, re-copy `src/`, `LICENSE.md`, `README.md` and the
normalized `Cargo.toml` from the new crate. Apply the manifest edits and the
patches above again, or drop the ones upstream now covers. Then run
`cargo test -p ubm-desktop` and regenerate the release artifacts
(`pnpm release:artifacts`).

## Patch 2: `corebluetooth-advertisement-extras`

**Problem.** Upstream parses the local name, manufacturer data, service data,
service UUIDs and TX power from `didDiscoverPeripheral:advertisementData:`.
It drops `CBAdvertisementDataSolicitedServiceUUIDsKey`,
`CBAdvertisementDataOverflowServiceUUIDsKey` and
`CBAdvertisementDataIsConnectable`. The legacy CoreBluetooth addon reported
all three (`native/electron/corebluetooth/index.js:99-110`).

**Change.**

- `src/corebluetooth/central_delegate.rs`:
  - New `pub fn advertisement_extras(&NSDictionary<NSString, AnyObject>) -> AdvertisementExtras`.
    A key the advertisement does not carry is `None`; a carried key with no
    entries is `Some(vec![])`.
  - The delegate sends the extras for each advertisement right after
    `DiscoveredPeripheral`, on the same ordered channel.
  - Correction (finding 122): this did not make `DeviceUpdated` see the
    advertisement's data. `DeviceUpdated` is dispatched from
    `DiscoveredPeripheral`, before the peripheral applies the extras and the
    other data, so a listener that read properties on that event could see
    the previous advertisement's data. Observations now come from patch 17's
    per-advertisement reports, which carry the extras themselves.
- `src/corebluetooth/internal.rs`: the event is forwarded to the peripheral.
- `src/corebluetooth/peripheral.rs`:
  - New `AdvertisementExtras` type.
  - The peripheral stores the latest advertisement's extras, readable through
    the inherent `Peripheral::advertisement_extras()`.
- `src/corebluetooth/mod.rs`: `central_delegate` is now `pub(crate)`.
- `src/platform.rs`: `AdvertisementExtras` and `advertisement_extras` are
  re-exported on Apple targets.
- `build.rs` lists `corebluetooth-advertisement-extras`.

**Tests.** `crates/ubm-desktop/tests/macos_advertisement_extras.rs` parses a
real `NSDictionary`, checking absent keys and present-but-empty versus
populated values. `tests/parity_ops.rs`
`advertisement_extras_reach_the_host_verbatim` covers the path through the
central. Physical check, which needs a Bluetooth-authorized macOS terminal
and a peer that solicits a service or advertises in the background: this has
not been run yet.

## Patch 3: `bluez-session-bus` (btleplug + `vendor/bluez-async`)

**Problem.** btleplug's BlueZ `Manager::new()` goes through
`bluez_async::BluetoothSession::new()`, which always connects to the D-Bus
system bus. The legacy BlueZ backend supported `busKind: 'session'`, used for
mock BlueZ services and sandboxed setups.

**Change.**

- `vendor/bluez-async` is bluez-async 0.8.2 from crates.io (checksum
  `84ae4213cc2a8dc663acecac67bbdad05142be4d8ef372b6903abf878b0c690a`,
  licence `MIT OR Apache-2.0` as its manifest declares). Its `[[example]]`
  manifest sections are removed; the examples are not vendored. It adds
  `BluetoothSession::new_session_bus()`, which is identical to `new()` but
  uses `dbus_tokio::connection::new_session_sync()`.
- `src/bluez/manager.rs` adds `Manager::new_session_bus()`.
- `build.rs` lists `bluez-session-bus`.
- Both crates are patched in the root, `native/tauri` and `example-tauri`
  workspaces.

**Tests.**

- `tests/parity_ops.rs` `bluez_bus_choice_is_carried_and_never_silently_replaced`
  checks the profile default and the selection rule. The session bus is only
  accepted on Linux with this patch.
- `a_session_bus_request_off_linux_fails_before_any_radio_opens` checks that
  an unhonourable bus fails the open before any radio is created.
- The Linux code paths are type-checked only from macOS; no dbus-daemon is
  available on this host.
- Linux host check, not yet run: start a session bus (`dbus-run-session`),
  export a mock `org.bluez` (for example python-dbusmock's `bluez5`
  template), and open with `CentralProfile { bluez_bus: BluezBus::Session, .. }`.
  The adapter listing, `adapter_state` and `security_state` must all answer
  from the mock.

## Patch 4: `corebluetooth-write-readiness`

**Problem.** btleplug uses `canSendWriteWithoutResponse` and
`peripheralIsReadyToSendWriteWithoutResponse:` only internally, for flow
control. The legacy CoreBluetooth backend exposed them as a readiness watch:
a probe plus readiness events (`connections.writeWithoutResponseReadiness`).

**Change.**

- `src/corebluetooth/internal.rs`:
  - New message `GetWriteReadiness` and reply `WriteReadiness(bool)`.
  - After the delegate's readiness callback drains the queued writes,
    `on_write_readiness` reports the readiness that is left to the
    peripheral.
- `src/corebluetooth/peripheral.rs`:
  - New inherent `can_send_write_without_response()`.
  - New `write_readiness_events()`, a broadcast; a lagging receiver sees
    `Lagged`, never a silent gap.
- `build.rs` lists `corebluetooth-write-readiness`.

**Tests.** `tests/parity_ops.rs`
`write_readiness_is_probed_and_reported_per_connection` covers the probe
admission and the event path through the central. Physical check, not yet
run: on a Bluetooth-authorized macOS terminal, a burst of writes without
response reports `ready: false` then `ready: true`.

## Patch 5: `winrt-scan-stopped`

**Problem.** btleplug never observes `BluetoothLEAdvertisementWatcher.Stopped`,
so a watcher the OS stops (for example when the radio goes off) looks like a
live scan. The legacy WinRT addon reported it (`OnScanTerminal`,
`winrt-boundary.inc:776`). Upstream also registered a new `Received` handler
on every `start()` without ever removing one, so after the first scan every
advertisement was delivered once per earlier scan.

**Change.**

- `src/winrtble/ble/watcher.rs`:
  - A `Stopped` handler is registered once and broadcasts `ScanStopped
    { error, error_name }` with the raw `BluetoothError` (0 = `Success`).
  - The `Received` registration token is kept and removed on `stop()` and
    before the next `start()`.
- `src/winrtble/adapter.rs` adds `Adapter::scan_stopped_events()`.
- `src/winrtble/mod.rs`: `ble` is now `pub(crate)`.
- `src/platform.rs` re-exports `ScanStopped` on Windows.
- `build.rs` lists `winrt-scan-stopped`.

**Tests.** `tests/parity_ops.rs` `an_os_ended_scan_is_released_and_reported`
covers the central side: an OS-ended scan releases the scan owner and is
reported with the OS's reason, and a report with no active scan ends
nothing. On the radio side, stops the radio requested itself are counted
and skipped. The Windows code is type-checked only. Windows host check, not
yet run: turn the radio off during a scan and confirm `scan_terminal_events`
reports `aborted` and a new scan starts; then run two consecutive scans and
confirm each advertisement arrives once.

## Patch 6: `attribute-instances` (all platforms)

**Problem.** Upstream kept one GATT attribute per UUID: CoreBluetooth
stored services, characteristics and descriptors in `HashMap<Uuid, _>`
(`corebluetooth/internal.rs`, `central_delegate.rs`), BlueZ kept "the first
characteristic of each UUID" and `HashMap<Uuid, _>` tables
(`bluez/peripheral.rs`), WinRT the same (`winrtble/peripheral.rs`). The API
types carried no instance, so two same-UUID siblings with equal facts
collapsed in the `BTreeSet` and every operation addressed "the" UUID. The
legacy backends kept every occurrence (`native/electron/corebluetooth/index.js`
occurrences, the BlueZ object-path model, the WinRT addon occurrences).

**Change.**

- `src/api/mod.rs`: `Service.instance`, `Characteristic.instance` /
  `service_instance`, `Descriptor.instance` / `service_instance` /
  `characteristic_instance`, `ValueNotification.instance` /
  `service_instance`. `instance` is the ATT handle where the OS reports one
  (BlueZ object path, WinRT `AttributeHandle`) and the discovery position
  otherwise (CoreBluetooth). Ordering is (UUID, instance), so per-UUID
  occurrence follows handle or discovery order.
- CoreBluetooth: `central_delegate::AttrKey { uuid, instance }` keys every
  table and message; `keyed()` numbers each discovery answer by array
  position; callbacks find an attribute's key by object identity in its
  parent array (`service_key` / `characteristic_key` / `descriptor_key`). An
  included service's attributes are ignored, as upstream never exposed them
  (upstream panicked when their UUID was unknown). `api_services` builds the
  discovery reply.
- BlueZ: `handle_from_object_path` reads the handle from `service%04x` /
  `char%04x` / `desc%04x`; tables are vectors keyed by (UUID, handle); a
  path without a handle or a descriptor listing that fails fails the
  discovery (upstream replaced a failed descriptor listing with an empty
  one). `mtu()` no longer unwraps a withheld `MTU` (upstream panicked).
- WinRT: see patch 9.

**Tests.**

- `corebluetooth::internal::ubm_instance_tests` (run on macOS with
  `cargo test -p btleplug --lib`): real `CBMutableService` /
  `CBMutableCharacteristic` / `CBMutableDescriptor` objects go through the
  keying, the table merge `set_characteristics` runs and the discovery reply:
  two same-UUID services each keep two identical same-UUID characteristics,
  a repeated discovery answer merges instead of duplicating, and two
  same-UUID descriptors key apart. (CoreBluetooth refuses two same-UUID
  descriptors on one local characteristic, so the descriptor case is keyed
  from an array directly.)
- `bluez::peripheral::ubm_instance_tests` (Linux only): handle parsing.
- `crates/ubm-desktop` `btleplug_backend::tests::f61_*`: identical same-UUID
  instances stay distinct and select in handle order; notifications route by
  instance. `tests/production_ingress.rs`
  `f61_same_uuid_instances_of_one_scope_route_to_their_own_forwarders` runs
  two real forwarders over one air: each instance's values reach only its
  own subscriber, so the former "ambiguous routing" refusal is gone.
  `os::bluez_model::every_instance_receives_its_own_facts_by_handle`: BlueZ
  characteristic flags map to every instance by handle.
- Physical checks, not yet run: a peripheral with two same-UUID services,
  characteristics and descriptors on macOS, Windows and Linux: discovery
  lists every instance; read, write and subscribe reach only the addressed
  one; notifications carry its instance.

## Patch 7: `central-state-detail`

**Problem.** `CentralState` had only `Unknown`, `PoweredOn`, `PoweredOff`.
CoreBluetooth's `Resetting`, `Unsupported` and `Unauthorized` became
`Unknown`, and only `PoweredOff` invalidated peripherals. The legacy
CoreBluetooth addon reported `resetting` / `unsupported` / authorization
`denied` (`index.js` `adapterSnapshot`) and treated every non-powered-on
state as an adapter loss.

**Change.** `CentralState::{Resetting, Unsupported, Unauthorized}`;
CoreBluetooth maps them; CoreBluetooth invalidates every peripheral on any
of them as it did on power-off. WinRT maps `Disabled` to `PoweredOff`
(patch 9). BlueZ reports `Powered` only.

**Tests.** ubm-desktop maps them to `AdapterPowerState` and
`tests/adapter_loss.rs` covers what each state does to admission and
teardown. Physical check, not yet run: toggle Bluetooth off/on and revoke
Bluetooth permission for the host app on macOS; the states arrive as
`powered-off`, `unauthorized`, `powered-on`.

## Patch 8: `scan-policy`

**Problem.** CoreBluetooth `start_scan` queued the request and returned
`Ok` even when the manager was not powered on, where CoreBluetooth ignores
it (the scan never runs, no error). CoreBluetooth always passed
`AllowDuplicatesKey: YES` and BlueZ always `DuplicateData: true` with
`Transport: auto` (BR/EDR inquiry added to a BLE scan). The legacy BlueZ
backend scanned `Transport: le` with `DuplicateData` from the caller's
policy (`bluez-runtime-models.ts` `scanFilterVariant`).

**Change.**

- `ScanFilter.allow_duplicates: Option<bool>` (`None` keeps upstream's
  every-advertisement default).
- CoreBluetooth: the start is answered by the CoreBluetooth thread and
  refused unless the manager is powered on; `AllowDuplicatesKey` follows
  `allow_duplicates`.
- BlueZ: `Transport: le`; `DuplicateData` follows `allow_duplicates`.
- WinRT has no duplicate filter: every advertisement is reported.
- ubm-desktop passes `allow_duplicates = (policy == all)`.

Owner decision pending: the legacy CoreBluetooth addon passed
`AllowDuplicatesKey: NO` for every policy (`addon.mm:782`); this patch gives
`all` its meaning (`YES`), at the cost of more traffic and power.

**Tests.** `tests/adapter_loss.rs`
`the_duplicate_policy_reaches_the_radio` (the policy reaches the radio's
filter). Physical checks, not yet run: on macOS, a scan started before
Bluetooth is on fails instead of silently not scanning; with `first` a
peer's repeated advertisements are coalesced by CoreBluetooth; on Linux,
`btmon` shows an LE-only discovery with `DuplicateData` following the
policy.

## Patch 9: `winrt-uncached-discovery`, `winrt-cccd-mode`, `winrt-adapter-by-id` (and the WinRT half of patch 6)

Written by the WinRT sub-lane; `build.rs` lists the three names, and the WinRT half of patch 6 ships under `attribute-instances`.

### WinRT half of patch 6: `attribute-instances`

**Problem.** Upstream `winrtble/peripheral.rs` kept "only the first
characteristic of each UUID" and keyed services, characteristics and
descriptors by UUID alone (`DashMap<Uuid, BLEService>`,
`HashMap<Uuid, BLECharacteristic>`, `HashMap<Uuid, BLEDescriptor>`). A
repeated service, characteristic or descriptor UUID was silently dropped or
overwritten, and every operation addressed "the" UUID. Upstream also left
patch #6's `instance` / `service_instance` / `characteristic_instance`
fields unset, so the WinRT backend did not compile.

**Change.**

- New `src/winrtble/gatt_model.rs` (std only): `index_unique` (index by
  key, a repeated key is an error, never an overwrite),
  `gatt_status_name` / `require_gatt_success` (names a
  `GattCommunicationStatus`).
- `ble/descriptor.rs`: `AttributeKey = (Uuid, u64)`. `BLEDescriptor` reads
  UUID and `GattDescriptor.AttributeHandle` at construction (fallible, no
  `unwrap`); `to_descriptor` fills `instance`, `service_instance`,
  `characteristic_instance`. Read/write take the `GattDescriptor` so no map
  lock is held across an await.
- `ble/characteristic.rs`: `BLECharacteristic` reads UUID,
  `AttributeHandle` and properties at construction; descriptors keyed by
  `AttributeKey`. Read/write/CCCD-write take the `GattCharacteristic`
  (no map guard across an await). `register` checks notify/indicate
  support before registering, and replaces a previous `ValueChanged`
  registration instead of leaking it (upstream re-subscribe accumulated
  handlers and duplicated values). `deregister(token)` rolls back a failed
  subscribe's own handler. `adopt_subscription` moves a live subscription
  to the object a later discovery returns for the same (UUID, handle). The
  notify handler is an `Arc<dyn Fn + Send + Sync>` so it can move.
- `ble/service.rs`: `BLEService { uuid, instance, characteristics:
  HashMap<AttributeKey, _> }`; `to_service` fills `instance`.
- `peripheral.rs`: `ble_services: DashMap<AttributeKey, BLEService>`.
  `read`, `write`, `subscribe`, `unsubscribe`, `read_descriptor`,
  `write_descriptor` look up (service UUID, service instance) → (UUID,
  instance) → (descriptor UUID, instance). A missing instance is
  `NotSupported("<kind> <uuid> (instance <n>) not found for <op>")`.
  Every `ValueNotification` carries `instance` and `service_instance`. A
  failed subscribe CCCD write removes its handler and reports a rollback
  failure together with the write failure.

**Tests.**

- `gatt_model.rs` unit tests (3): every failure status is named, an unknown
  status is reported by number, repeated UUIDs stay distinct by handle, a
  repeated handle is refused. They run on every host through
  `crates/ubm-desktop/tests/winrt_gatt_model.rs`, which compiles the same
  file with `#[path]` (passes on macOS).
- Everything that touches WinRT objects is type-check-only here.

**Windows host check still to run.** A peripheral with two same-UUID
characteristics in one service (and two same-UUID services): discovery lists
both with their handles as `instance`; read/write/subscribe of each reaches
only that handle; notifications of each arrive with their own `instance`.
Same for two same-UUID descriptors (read/write each).

### `winrt-uncached-discovery`

**Problem.** Upstream `ble/device.rs` discovered services with
`BluetoothCacheMode::Cached`, and characteristics/descriptors `Uncached`
with a 5 s timeout that fell back to `Cached` and only logged a warning. A
non-success `GattCommunicationStatus` from characteristic discovery became
an empty list; `peripheral.rs` dropped a whole service whose characteristic
query failed (`warn!` only) and turned a descriptor failure into an empty
descriptor list. `discover_services` only added services whose UUID was not
yet known, so after `GattServicesChanged` changed or removed services stayed
stale. The legacy addon (`winrt-boundary.inc` `Discover`, ~lines 1060-1100,
and connect confirmation ~898) queried every level `Uncached`, required
`Success` at every level (`RequireSuccess`), and replaced its cache.

**Change.**

- `ble/device.rs`: `GATT_CACHE_TIMEOUT` and the cached fallback removed.
  Services, characteristics and descriptors are queried `Uncached` every
  time. Any non-success status is `Err("<stage> failed with
  GattCommunicationStatus <Name> (<raw>)")`; a status that cannot be read is
  an error too. `discover_services` returns the fresh service list.
- `peripheral.rs` `discover_services`: builds the complete table first
  (any failed service/characteristic/descriptor query fails the whole
  discovery, with the service and characteristic UUID and handle in the
  message, leaving the previous table untouched), then REPLACES the table:
  services absent from the new database are removed, changed ones replaced.
  A live subscription whose (service, characteristic) UUID and handle
  survive is moved to the new GATT object (new registration made before the
  old one is removed); failures to move are returned as the discovery's
  error. A subscribed attribute that disappeared ends with it (logged;
  the desktop core already learns of the change from `GattServicesChanged`).

**Tests.** `gatt_model` status tests above. The replace/adopt path needs
WinRT objects: type-check-only.

**Windows host check still to run.** (1) A peripheral that answers a GATT
query with an error (e.g. an encrypted service while unpaired):
`discover_services` fails naming `AccessDenied`/`ProtocolError`, never
succeeds with a missing service. (2) With a device whose firmware can change
its GATT table: discover, change the table, observe `ServicesChanged`,
discover again: removed services are gone and new/changed ones appear with
their new handles. (3) Discover twice while subscribed: notifications keep
flowing after the second discovery, with no duplicates after it.

### `winrt-cccd-mode`

**Problem.** `subscribe` writes `Indicate` whenever the characteristic can
indicate. ubm-desktop rewrote the CCCD with its own
`GetGattServicesForUuid`/`GetCharacteristicsForUuid` lookup (`Cached`), and
refused any repeated service or characteristic UUID as ambiguous.

**Change.** `peripheral.rs`: inherent
`Peripheral::write_client_configuration(&Characteristic,
GattClientCharacteristicConfigurationDescriptorValue)`, which writes the CCCD
of exactly that instance on the same `GattCharacteristic` object the
subscription registered on. ubm-desktop's lookup and its ambiguity refusal
are removed (`os::windows::write_cccd`).

**Tests.** Type-check-only.

**Windows host check still to run.** Subscribe to a characteristic with both
notify and indicate with a notify preference: the peer's CCCD reads `0x0001`
afterwards (and `0x0002` when indication is required). Repeat on the second
of two same-UUID characteristics: only its CCCD changes.

### `winrt-adapter-by-id`

**Problem.** btleplug builds one adapter per `Radio` in
`Radio::GetRadiosAsync` order, with no link to a `BluetoothAdapter` device
id; ubm-desktop matched the default adapter's radio by name and refused any
non-default adapter id. `get_central_state` mapped `RadioState::Disabled` to
`Unknown` (the legacy addon's `RadioPower`, `addon.cpp:203-213`, maps it to
off). `Adapter::new` never removed its `StateChanged` registration (every
adapter ever built kept emitting into a dead manager) and only
`eprintln!`ed a failed registration.

**Change.** `adapter.rs`:

- `Adapter::from_radio(Radio)` (what `Manager::adapters` uses too) and
  `Adapter::without_radio(CentralState)` for an adapter whose radio the OS
  withholds (legacy `SelectAdapter` selected it without a radio when access
  was not granted); `adapter_state` answers that state.
- `Adapter::rebind_radio(Radio)`: after the adapter device comes back, report
  state from the new `Radio` and emit its current state.
- The radio binding removes its `StateChanged` registration on drop; a
  failed registration is an error.
- `RadioState::On` → `PoweredOn`, `Off | Disabled` → `PoweredOff`, else
  `Unknown`. A state read failure is an error from `adapter_state`, and
  `Unknown` (logged) in the change handler.

**Tests.** ubm-desktop `os::winrt_model` tests `select_listed`,
`radio_access`, `AdapterPresence`, `deployment_from_status` (run on macOS).
The WinRT calls are type-check-only.

**Windows host check still to run.** See the lane report's physical list
(select a USB dongle that is not the default; power it off/on; unplug and
replug it; packaged vs unpackaged `deployment`).

## Patch 10: `stream-lag-reported`

**Problem.** Every platform fans notifications out through a bounded tokio
broadcast (16 slots per peripheral on CoreBluetooth and WinRT) and adapter
events through another (16 slots, `common/adapter_manager.rs`). Both
consumers turned `RecvError::Lagged(n)` into nothing
(`filter_map(|x| x.ok())`, `common/util.rs`), so a receiver that fell
behind lost values, connects, disconnects or state changes with no report.

**Change.**

- `ValueNotification.lost_before`: notifications this receiver missed
  immediately before this one (their characteristics are unknown).
  `notifications_stream_from_broadcast_receiver` carries every `Lagged(n)`
  into the next delivered notification.
- `CentralEvent::EventsLost { skipped }` replaces the silent gap on the
  adapter event stream.
- `btleplug::ubm::notifications_stream_from_broadcast_receiver` exports the
  real notification stream for the desktop core's tests.
- ubm-desktop: each subscription's forwarder reports its receiver's loss as
  `RadioEvent::NotificationsLost` on its own scope (any lost value may have
  been its own); the central accounts it on the hub by each consumer's
  overflow policy (`Central::deliver_upstream_loss`: `error` — the desktop
  policy — ends the stream with an `overflow` terminal counting the lost
  items; lossy policies count them, `upstream_lost_count`). Lost adapter
  events are counted (`ResourceCounters::radio_events_lost`) and logged.
  BlueZ notifications arrive over bluez-async's unbounded D-Bus stream and
  never lag.

**Tests.**

- `common::util::ubm_lag_tests` and `common::adapter_manager::ubm_lag_tests`
  (`cargo test -p btleplug --lib`): a real broadcast overrun reports the
  exact number missed; a receiver that keeps up loses nothing.
- `crates/ubm-desktop/tests/notification_loss.rs`: a lag forced through the
  real broadcast, the real notification stream and the real production
  forwarder arrives as `NotificationsLost { lost: 3 }` before the next
  value; the central ends the subscription with an `overflow` terminal of 3
  (a stale-epoch report is ignored); lost adapter events are counted.
- `ubm-core` `upstream_loss_follows_each_consumers_overflow_policy`.
- Physical check, not yet run: on Windows or macOS, subscribe to a
  high-rate characteristic and stall the host process (debugger break) for
  a few seconds; the subscription ends `overflow` with the lost count
  instead of silently skipping values.

## Patch 11: `winrt-passive-scan`

**Problem.** Upstream's `BLEWatcher::start` forced
`BluetoothLEScanningMode::Active` and `AllowExtendedAdvertisements(true)`
(its failure ignored). The legacy WinRT addon scanned with a fresh
watcher's defaults: passive, no extended advertisements
(`winrt-boundary.inc`). Active scanning sends a scan request to every
advertiser, which costs radio traffic and power (finding 86, N7). No public
scan option chooses active or passive, so the legacy default applies.

**Change.** `BLEWatcher::configure_for_scan` (called by `start`) clears the
OS service filter as before and sets `Passive`. Nothing sets extended
advertisements, so they stay at the watcher default (off). As with the
legacy addon, a peer's scan-response data (often its name) is not
requested.

**Tests.** `winrtble::ble::watcher::ubm_scan_mode_tests`
(`cargo test -p btleplug --lib` on Windows): a configured watcher reads back
`Passive` and extended advertisements off. On macOS and Linux this is only
compile-checked for the Windows targets. Physical check, not yet run: on
Windows, a scan of a peer that answers scan requests shows no
`SCAN_REQ` in an air trace.

## Patch 12: `bluez-name-pattern`

**Problem.** The legacy BlueZ backend put the caller's local-name prefix in
the `SetDiscoveryFilter` `Pattern` (`bluez-runtime-models.ts`
`scanFilterVariant`). `ScanFilter` had no name field, so the Rust path
filtered by name in software only: the same results, with more device
objects and more D-Bus traffic (finding 89, N10).

**Change.** `ScanFilter.name_prefix: Option<String>` (`serde(default)`).
BlueZ passes it as `DiscoveryFilter.pattern`, which vendored bluez-async
already sends as `Pattern`; `start_scan` builds the filter in
`discovery_filter` (together with patch 8's transport and duplicate
settings). BlueZ's `Pattern` also matches an address prefix, so it only
narrows and the host keeps its name match. CoreBluetooth and WinRT have no
name filter and ignore it, as the legacy backends did. ubm-desktop passes
`ScanFilterSpec.name_prefix` from `DesktopCentral::start_scan_matching`.

**Tests.** `bluez::adapter::ubm_discovery_filter_tests`
(`cargo test -p btleplug --lib` on Linux; compile-checked elsewhere): the
prefix becomes `pattern`, and no prefix sets none.
`crates/ubm-desktop/tests/adapter_loss.rs`
`the_name_prefix_reaches_the_radio`: the prefix reaches the radio's filter
and an empty prefix is `argument.invalid`. Physical check, not yet run: on
Linux, `dbus-monitor --system` shows `Pattern` in `SetDiscoveryFilter`.

## Patch 13: `event-capacity`

**Problem.** The adapter event broadcast (`common/adapter_manager.rs`) and
each peripheral's notification broadcast (CoreBluetooth
`corebluetooth/peripheral.rs`, WinRT `winrtble/peripheral.rs`) held 16
events. A receiver more than 16 behind lost events. Patch 10 reports that
loss, but 16 is far below what the legacy backends queued before losing
anything: the CoreBluetooth addon's thread-safe functions had no bound, and
the WinRT addon queued 128 notifications and 256 advertisements
(`native/electron/winrt/src/addon.cpp`). This was a loss point below legacy
(finding 107 audit).

**Change.** `btleplug::ubm::EVENT_CAPACITY` (4096) sizes those three
broadcasts. Loss past it is still reported (patch 10).

**Tests.** `common::adapter_manager::ubm_lag_tests` (`cargo test -p
btleplug --lib`): `EVENT_CAPACITY + 4` events before the reader reads
report exactly 4 lost. The test also asserts the capacity is at least
256.

## Patch 14: `corebluetooth-read-notify`

**Problem.** CoreBluetooth reports a read response and a notification
through the same callback (`peripheral:didUpdateValueForCharacteristic:error:`).
Upstream handed the next value update to whichever read was waiting
(`corebluetooth/internal.rs` `on_characteristic_read`) and never said so. A
notification could come back as a read result, and the real read response
was then delivered as a notification (finding 110).

Upstream also ignored the `NSError` of read, write and notification-state
callbacks. A failed read, write or enable was never answered, so it waited
until a deadline. A failed descriptor read or write panicked
(`reply => panic!`).

The first version of this patch refused the ambiguous cases as the legacy
addon did (413 read while notifying, 414 overlapping read, 415 subscribe
during a read). 5.0 replaces the refusal: a read while notifying must work
with the same application code as Android (the Polar H10 PMD control point
is subscribed, then read).

**Change.**
- `api::ReadProvenance` (`ReadResponse`, `ReadOrNotification`) says what a
  characteristic read value is.
- `corebluetooth::Peripheral::read_with_provenance` (inherent) returns the
  value and its provenance; `api::Peripheral::read` returns the value of the
  same read.
- `read_value` and `subscribe` no longer refuse: every read calls
  `readValueForCharacteristic`, and waiters complete in request order.
- `corebluetooth/read_notify.rs` `ReadNotifyState::route_value`: a value
  update completes the oldest pending read as `ReadResponse` when the
  characteristic cannot notify, and as `ReadOrNotification` when it notifies
  or a notification state change is in flight. A value that may be a
  notification is also sent to the notification stream; with no read
  pending every update is a notification, as upstream.
- A read whose future was dropped (timeout, cancel) keeps its waiter in the
  queue, so its update never completes a later read.
- An unsubscribe callback that answers a pending enable fails it with 411.
- Every attribute callback error now answers its waiter:
  - `CentralDelegateEvent::AttributeFailed` carries the error, and
    `on_attribute_failed` delivers it (patch 15);
  - a failed descriptor read or write returns an error instead of
    panicking.

WinRT (`ReadValueAsync`, uncached) and BlueZ (`ReadValue`) answer a read
with its own response; `crates/ubm-desktop` reports `ReadResponse` for them.

**Tests.** `corebluetooth::read_notify::tests` cover the routing;
`corebluetooth::internal::ubm_instance_tests::queued_reads_complete_in_order_as_read_responses`
and `a_notification_before_the_read_reply_is_reported_ambiguous_and_still_notified`
drive the waiter queue (`cargo test -p btleplug --lib`, macOS). Physical
check, not yet run: on macOS, subscribe to the Polar H10 PMD control point,
then read it. The read succeeds as `read-or-notification`, and the
subscription keeps receiving values.

## Patch 15: `platform-errors`

**Problem.** Every platform failure reached hosts as text:
- CoreBluetooth replies were `Err(String)`;
- WinRT statuses and HRESULTs were `Error::Other(format!(..))`;
- BlueZ D-Bus errors were `Error::Other(Box<BluetoothError>)`.

The legacy backends reported typed identities, and applications branched
on them (finding 113):
- CoreBluetooth `{domain:"corebluetooth", code:<NSError code>}`;
- WinRT `{domain:"winrt", code:"gatt-status"|"hresult", metadata:{gattStatus|hresult}}`;
- BlueZ `{domain:"bluez-dbus", code:<D-Bus error name>}`.

**Change.**
- New `btleplug::PlatformError { domain, code, message, metadata }` and
  `Error::Platform`.
- CoreBluetooth:
  - `CoreBluetoothReply::Failed(PlatformError)` is built from the callback's
    `NSError`: code is the error code, metadata `nsErrorDomain`;
  - the patch-14 refusals use codes 413, 414, 415 and 411 with
    `nsErrorDomain` `UBMCoreBluetooth`;
  - `didFailToConnectPeripheral:error:` fails the connect with that
    `NSError` too (upstream kept only its localized description), so a
    `CBError.connectionTimeout` (6) or `connectionFailed` (10) is
    recognizable as a link that was not established.
- WinRT:
  - a non-success `GattCommunicationStatus` is `code:"gatt-status"` with
    `gattStatus` set to `success`, `unreachable`, `protocol-error`,
    `access-denied` or `unknown`;
  - a connect whose `GetGattServicesAsync` answers `Unreachable` fails with
    that answer (`gatt-status` `unreachable`) instead of
    `Error::NotConnected`; the other connect statuses keep upstream's
    mapping;
  - a `windows::core::Error` is `code:"hresult"` with `hresult` as
    `0xXXXXXXXX`;
  - the helpers `gatt_status_code` and `hresult_code` live in the pure
    `winrtble/gatt_model.rs`, and `btleplug::ubm::hresult_code` exposes the
    HRESULT format on Windows.
- BlueZ: `BluetoothError::DbusError` is `PlatformError::bluez_dbus(name,
  message)`, with `org.bluez.Error.Failed` when D-Bus gave no name.
- ubm-desktop copies the answer into `DesktopError::platform()`
  (`PlatformDetail`). A BlueZ D-Bus failure takes the legacy BlueZ identity
  `platform.failure`.

**Tests.**
- `ubm_platform_error_tests` (btleplug lib): BlueZ names.
- `crates/ubm-desktop/tests/winrt_gatt_model.rs` runs on every host: the
  legacy WinRT codes.
- `btleplug_backend::tests::f113_platform_answers_become_typed_error_fields`:
  each host's answer becomes the typed fields, and BlueZ becomes
  `platform.failure`.
- `tests/parity_ops.rs` `the_platform_answer_survives_the_central`: the
  answer survives the central on read, dispatched write and connect.
- `tests/parity_ops.rs`
  `a_transient_connect_failure_is_caller_decides_through_the_central` and
  `errors::tests::a_transient_link_establishment_failure_is_caller_decides`:
  the connect answers above are `caller-decides` (owner decision, 5.0).
- Physical check, not yet run: a peer that refuses a read with an ATT error
  reports its code on each host.

## Patch 16: `winrt-service-filter`

**Problem.** The legacy WinRT addon put the caller's service UUIDs on the
watcher's OS filter (`winrt-boundary.inc`:
`AdvertisementFilter().Advertisement().ServiceUuids().Append`). Upstream
btleplug cleared that filter and matched service UUIDs in software only
(finding 117). That meant more radio and CPU work. It also changed behaviour
for advertisements that carry the UUID only in a scan response.

**Change.** `BLEWatcher::configure_for_scan(services)` clears the OS filter
from the previous scan, then appends each requested service UUID, as the
legacy addon did. The software predicate in the `Received` handler stays as
the final gate. Upstream's reason for dropping the OS filter was that some
Windows drivers drop matching 128-bit advertisements. That is legacy WinRT
behaviour too, so the 4.x behaviour is kept.

**Tests.** `winrtble::ble::watcher::ubm_scan_mode_tests::the_service_filter_reaches_the_os_watcher`
(Windows, `cargo test -p btleplug --lib`) reads the watcher's filter back:
the requested UUIDs, in order, and an empty filter after a later scan
without services. On macOS and Linux this is compile-checked for the
Windows targets only. Physical check, not yet run: on Windows, a filtered
scan reports only advertisers of the requested services.

## Patch 17: `advertisement-reports`

**Problem (findings 120, 122).**
- Upstream raised events for only some sightings, and listeners read the
  peripheral's merged properties afterwards:
  - **CoreBluetooth:** `DeviceUpdated` fired only for peripherals with a
    name. An unnamed peripheral was reported once and then never again,
    including in later scans.
  - **BlueZ:** there was no `DeviceUpdated` at all. A device BlueZ already
    knew was invisible to every scan after the first.
  - **WinRT:** events fired per advertisement, but properties merged across
    advertisements: the local name stuck, services accumulated, and service
    data was replaced.
- On CoreBluetooth the update was dispatched before the advertisement's
  data was applied (see the patch 2 correction).
- A short WinRT service-data section panicked (`split_at`).
- The legacy backends reported every sighting with its own data:
  - CoreBluetooth: every `didDiscoverPeripheral` callback;
  - WinRT: every `Received` event;
  - BlueZ: the device state, on discovery, on every property change and for
    every known device when a scan starts.

**Change.**
- New `api::AdvertisementReport` holds `source`, name, RSSI, TX power,
  manufacturer data, service data, services and the CoreBluetooth extras.
- New `ReportSource` values: `Advertisement` (this advertisement's own data)
  and `DeviceState` (the OS's merged state).
- New events:
  - `CentralEvent::Advertisement { id, report }`;
  - `CentralEvent::AdvertisementUnread { id, detail }`, for a sighting whose
    data the OS could not return, so it is reported instead of dropped.
- **CoreBluetooth:**
  - The delegate builds a report from each callback's `advertisementData`.
    The name is the advertised name, else the peripheral's, as the legacy
    addon did.
  - The report is sent after the events that update the merged state, as
    `CoreBluetoothEvent::Advertised`, and the adapter emits it.
  - Every rediscovery raises `DeviceUpdated`, named or not.
- **WinRT:**
  - `Peripheral::advertisement_report` reads the received event itself, and
    the adapter emits the report, or `AdvertisementUnread`, for every
    advertisement.
  - Service-data sections parse through the pure
    `gatt_model::service_data_section`, which skips a short section instead
    of panicking; this also applies to the merged properties.
- **BlueZ:** discovery, RSSI, manufacturer data, service data and service
  changes each also emit a `DeviceState` report built from `Device1`, or
  `AdvertisementUnread`.
- **ubm-desktop:**
  - Observations come only from reports, labelled
    `PeerSnapshot.extras.source` (`ObservationSource`).
  - On Linux, every device BlueZ knows is reported when a scan starts, as
    the legacy backend did.
  - Unreadable sightings are counted (`os_adapter_failures()
    .advertisement_read_failures`) and logged.

**Tests.**
- `winrtble::gatt_model` `service_data_sections_parse_by_uuid_width` runs on
  every host through `crates/ubm-desktop/tests/winrt_gatt_model.rs`.
- `bluez::adapter::ubm_sighting_tests` (Linux) checks that a BlueZ sighting
  is the labelled device state.
- `btleplug_backend::tests::f122_a_sighting_carries_its_own_data_and_label`
  checks the observation carries the report's data and label.
- The CoreBluetooth and WinRT callback wiring is compile-verified only.
- Physical checks, not yet run:
  - on macOS, an unnamed peripheral is observed on every advertisement and
    in a second scan;
  - on Linux, a paired device is observed when a second scan starts;
  - on Windows, two advertisements with different manufacturer data each
    carry their own.

## Patch 18: `bluez-device-changes` (vendored bluez-async + btleplug)

**Problem.** bluez-async raised device events only for `Connected`,
`RSSI`, `ManufacturerData`, `ServiceData`, `UUIDs` and `ServicesResolved`
(`vendor/bluez-async/src/events.rs`). A change to `Name`, `Alias`,
`TxPower`, `Appearance`, `AddressType`, `Class` or any other `Device1`
property raised nothing. A device whose name arrived in a later signal was
therefore never reported with it (finding 125). Patch 17 also raised one
sighting per per-property event, so a signal that changed several
properties produced several. The legacy BlueZ backend reported the device
once on every `Device1` `PropertiesChanged` signal
(`bluez-backend-runtime.ts:345-349,380-384`).

**Change.**
- `vendor/bluez-async`: new
  `DeviceEvent::PropertiesChanged { properties }`. It is raised once per
  `Device1` `PropertiesChanged` signal, after the specific events, with the
  changed and invalidated property names sorted. `DeviceEvent` is
  `#[non_exhaustive]`, so existing matches are unaffected.
- `vendor/btleplug` BlueZ adapter: a sighting (patch 17's
  `DeviceState` report) comes from `Discovered` and from each
  `PropertiesChanged`: one per signal, a name-only change included. The
  per-property events keep their upstream mapping but no longer add
  sightings.
- The scan gate is ubm-desktop's: a sighting becomes an observation only
  while a scan runs (finding 121).
- `build.rs` lists `bluez-device-changes`.

**Tests.**
- `vendor/bluez-async/src/events.rs`:
  - `device_name_alias_and_other_changes_are_reported`: a name-only change,
    an alias-only change and a mixed change with an invalidated property
    each raise one summary with the sorted names.
  - The existing RSSI, manufacturer-data, service-data and UUID tests now
    expect the summary after their specific event.
- `vendor/btleplug/src/bluez/adapter.rs`
  `every_device_signal_is_one_sighting`: discovery and the summary are
  sightings; per-property events are not.
- Both are Linux-only. On macOS they are type-checked for
  `x86_64-unknown-linux-gnu`; bluez-async's tests were checked standalone,
  since it is not a workspace member and libdbus is absent.
- Physical check, not yet run: on Linux, a device whose name arrives in a
  later scan response is observed with the name during the scan.

## Patch 19: `disconnect-lifecycle` (vendored btleplug + bluez-async)

**Problem.**
- **The peripheral is forgotten at disconnect.** `AdapterManager::emit`
  removed a peripheral on `DeviceDisconnected`. After any disconnect on
  macOS and Windows, a reconnect by the same id answered `peer.not-found`
  until a new scan found the device again (finding 127). A local disconnect
  that raced a remote loss could itself answer `peer.not-found`. The legacy
  addons resolved the device again: CoreBluetooth
  `retrievePeripheralsWithIdentifiers` (`addon.mm:796-846`), WinRT
  `FromBluetoothAddressAsync`.
- **Pending work hangs at disconnect.** CoreBluetooth's
  `confirm_disconnect` answered characteristic waiters only. A pending
  service discovery, descriptor read and descriptor write waited forever.
  The legacy addon's `failPendingForDevice` answered all of them (finding
  130).
- **BlueZ waits out a disconnect.** bluez-async's connect waited for
  `ServicesResolved` and ignored a disconnect, so a link lost during a
  connect reported a service-discovery timeout 5 s later.

**Change.**
- `common/adapter_manager.rs`: a disconnect no longer removes the
  peripheral. Only a power-off drops the CoreBluetooth side's own state.
  `replace_peripheral` overwrites an entry that the OS resolved again.
- CoreBluetooth:
  - A connect to an identifier this central no longer holds (after a
    power-off or reset) retrieves it with
    `retrievePeripheralsWithIdentifiers`. Upstream never answered that
    connect.
  - `Central::add_peripheral(id)` returns the known peripheral, or retrieves
    it through the new `ResolvePeripheral` message.
  - `confirm_disconnect` now also answers, with the disconnect error:
    - the pending connect;
    - the pending service discovery;
    - every descriptor read and write waiter.

    It then clears the attribute database, and a reconnect discovers it
    again.
- WinRT: `Central::add_peripheral(address)` returns the known peripheral, or
  opens a new one by Bluetooth address.
- `vendor/bluez-async`: `service_discovery_outcome` ends a pending discovery
  on the device's `Connected { connected: false }` with the new
  `BluetoothError::DisconnectedDuringServiceDiscovery`.
- `vendor/bluez-async` (B-R5): `connect_with_timeout` tolerates
  `org.bluez.Error.AlreadyConnected` from `Device1.Connect` — the
  connect's own answer that the link exists, as the legacy backend's
  `connectBluezPhysicalLink` did. Upstream failed the connect.
- `vendor/bluez-async` (B-R6): `disconnect` confirms the link ended after
  the `Disconnect` method returns: `Connected` reads `false` (or the
  device object is gone — `UnknownObject`/`DoesNotExist`, the same answer
  — as legacy's Tauri path classified it), within the legacy 1 s bound
  (`DISCONNECT_CONFIRMATION_TIMEOUT_MS`). An unconfirmed link reports the
  new `BluetoothError::DisconnectConfirmationTimedOut` and stays pending,
  never released. Upstream returned after the method call.
- ubm-desktop resolves a peer id it does not hold through `add_peripheral`
  (CoreBluetooth identifier, WinRT address). On Linux, BlueZ keeps the
  device object.
- ubm-desktop `disconnect` goes straight to the radio (no pre-disconnect
  `is_connected()` query, as legacy did) and reports a removed device
  object (`UnknownObject`/`DoesNotExist`, T-R1) as released.
- `build.rs` lists `disconnect-lifecycle`.

**Tests.**
- `common/adapter_manager.rs` `a_disconnected_peripheral_stays_known`
  (Apple): a real CoreBluetooth `Peripheral` survives a
  `DeviceDisconnected` emit.
- `corebluetooth/internal.rs` `a_disconnect_answers_every_attribute_waiter`:
  characteristic and descriptor waiters on real CoreBluetooth attribute
  objects all receive the disconnect error.
- `vendor/bluez-async/src/lib.rs`:
  - `a_disconnect_ends_a_pending_service_discovery`;
  - `resolved_services_end_the_discovery`;
  - `an_already_connected_device_is_connected` (B-R5);
  - `a_gone_device_object_confirms_the_disconnection` (B-R6).

  These are type-checked for `x86_64-unknown-linux-gnu` only (no libdbus on
  macOS).
- ubm-desktop `f127_a_peer_id_names_its_os_identity`: a peer id parses to
  the platform identifier.
- ubm-desktop `f127_a_listed_identity_resolves_without_a_scan`,
  `t_r1_a_gone_device_object_confirms_the_peer_is_released`,
  `t_r1_a_transport_failure_is_not_release_evidence`;
  `os::winrt_model::tests::a_listed_address_reopens_without_a_scan`;
  `os::bluez_model::tests::pairing_is_possible_as_legacy_reported_it` (B-R1).
- `corebluetooth/internal.rs` `an_unknown_identifier_resolves_without_a_scan`
  and `a_disconnect_of_an_unknown_peripheral_sends_no_event` (finding 127,
  Apple): the `ResolvePeripheral` / `retrievePeripheralsWithIdentifiers`
  path answers not-found without a scan; an unknown disconnect publishes
  nothing.
- Physical checks, not yet run:
  - on macOS and Windows: connect, disconnect, reconnect without a scan
    (`cargo test -p ubm-desktop --test reconnect_without_rescan -- --ignored`
    covers the never-observed miss path with an adapter only);
  - on macOS: disconnect during discovery and during a descriptor read;
  - on Linux: connect to an already-connected device, disconnect
    confirmation (gone object and 1 s bound).
