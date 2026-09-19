# HOST-DESKTOP btleplug capability-vs-frozen-matrix parity gap list

Status: open gaps, not re-described closure. Every required desktop central
capability maps to exactly one verdict:

- `btleplug-provides` — implemented through a supported btleplug 0.12 path
  in `crates/ubm-desktop`; reported `limited` (deterministic-only) until
  physical-radio evidence qualifies it.
- `narrow-OS-adapter-needed` — open work: a narrow per-OS adapter on top of
  btleplug. The row stays unsupported; nothing is re-described away.
- `preapproved-limitation-candidate` — explicitly bounded behavior with a
  named limitation, never silent downgrade. The gap stays open until the
  limitation is approved.

Scope: required **desktop central** capabilities (frozen matrix as consumed
by `native/tauri/src/capabilities.rs` plus UBM 5.0 plan §§9/11.2). Mobile-only
roles (`background:apple-restoration`,
`background:android-connected-device-service`) are out of the desktop scope
and are not carried as desktop rows — that scoping is explicit here, not a
shrinking of the matrix. Every other frozen row is carried, including
`peer:restored` (desktop processes start without restored handles, so it
reports as adapter-needed work, never as silently unsupported).
Peripheral/server roles are required 5.0 outcomes
but are NOT central capabilities: btleplug is a central-mode library, so
every server/advertising row is a separate adapter track, not a row below.

Evidence level of this report: adapter-compile plus deterministic
boundary-fault receipts (`cargo test -p ubm-desktop`, mocked btleplug
boundary only). NO BLE hardware exists on this host, so there is deliberately
no radio proof here: every `btleplug-provides` row is `limited`, and the
physical central slice (macOS/Windows/Linux read/notify/cleanup on real
radios) is explicitly queued as the boundary. Host build receipts so far:
Linux `cargo check`/`cargo test` for the workspace crate; the production
`BtleplugRadio` compiles against btleplug 0.12 on Linux (BlueZ). macOS
(CoreBluetooth) and Windows (WinRT) backend paths compile through the same
host-neutral code but have no build or radio receipt from this host.

The same table ships as code in
`crates/ubm-desktop/src/capabilities.rs` (`DESKTOP_CAPABILITIES`) and is
projected into the live core by `register_desktop_capabilities`, so runtime
capability truth matches this report row for row.

## btleplug-provides (6)

| Capability | Scenario | Bound |
|---|---|---|
| `discovery:continuous-scan` | scan.owner-join-authority-and-signature | One global scan owner (`one-global-scan-owner`); explicit stop plus event-source-close settlement; second owner fails `scan.already-active` before any radio effect. |
| `peer:resolve-reference` | peer.resolve-reference | platform-guid resolution for observed peers (`platform-guid-only`); address domains need OS identity adapters. |
| `connection:direct` | connection.lease-joins-borrowing-transfer-and-revocation | Direct connect plus ownership cleanup (`deterministic-only`); half-open radio links are disconnected on failure; disconnect waits are bounded (1 s) and failures retained, never reported clean. |
| `connection:rssi` | connection.rssi-and-att-mtu-capability-contract | RSSI reported only when the OS measures the link (`deterministic-only`): `CBPeripheral.readRSSI` through `RadioBoundary::read_rssi` → `DesktopCentral::read_rssi` (lease holder, connected link, bounded by the budget). Windows and Linux answer `capability.unsupported` (see Per-OS verdicts). |
| `gatt:descriptors` | gatt.descriptor-discovery-read-write | Descriptor discovery/reads/writes with occurrence identity (`deterministic-only`); direct CCCD writes fail `gatt.cccd-managed`, sharing stays with subscribe/unsubscribe. |
| `gatt:indications` | gatt.indications | Subscribed values buffer per consumer and are observable through the take API (`delivery-kind-unknown`: the btleplug stream does not distinguish indications from notifications; per-value delivery kind stays `unknown`; the enable's CCCD mode is answered before any effect from the characteristic's properties and the platform rule (`delivery::plan_delivery`, finding 39), and the Windows adapter rewrites the CCCD to honour a requirement). Per-instance routing by (service, occurrence, characteristic, occurrence): every notification carries its attribute instance (vendored btleplug patch 6, `attribute-instances`: ATT handle on BlueZ and WinRT, discovery position on CoreBluetooth), so same-UUID instances subscribe side by side and each value reaches only the instance that fired it. |

## narrow-OS-adapter-needed (22)

| Capability | Scenario | Missing adapter |
|---|---|---|
| `scan:platform-options` | scan.platform-options | Active/passive/PHY scan knobs are not exposed; every OS scans with the legacy defaults (WinRT passive without extended advertisements, vendored patch 11, and the caller's service UUIDs on the OS watcher filter, patch 16). BlueZ receives the caller's name prefix as `Pattern` (patch 12). |
| `peer:restored` | peer.restored | Adopting OS-restored peers across process restarts; desktop processes start without restored handles. |
| `peer:address-targeting` | peer.address-targeting | OS peer identity (CoreBluetooth hides addresses entirely). |
| `peer:known` | peer.known-peers | OS-known peer retrieval. |
| `peer:system-connected` | peer.system-connected | Adopting OS-connected peripherals. |
| `peer:bonded` | peer.bonded | OS bond-store readout. |
| `connection:when-available` | connection.when-available | Deferred auto-connect / reconnect daemon path. |
| `connection:effective-mtu` | connection.rssi-and-att-mtu-capability-contract | The OS-measured MTU already feeds every write through `mtu()-3` into the core maximum-write-length (fail-closed when unmeasured); exposing the negotiated value as a host read needs an adapter. |
| `connection:request-mtu` | connection.mtu-request | OS MTU-request control path. |
| `connection:priority` | connection.priority | OS connection-priority control. |
| `connection:parameters` | connection.parameters | OS connection-parameter update. |
| `connection:phy` | connection.phy | OS PHY selection. |
| `connection:subrate` | connection.subrate | OS subrate control. |
| `security:state` | security.state | Observed link-security readout (never inferred from a flag). |
| `security:pair` | security.pair | OS-mediated pairing ceremony. |
| `security:cancel-pairing` | security.cancel-pairing | Pairing cancellation on the pairing adapter. |
| `security:unpair` | security.unpair | Per-OS remove-bond. |
| `security:custom-ceremony` | security.custom-ceremony | Reviewed profile plus adapter; no handshake is invented here. |
| `security:pairing-generation` | security.pairing-generation | Bond-generation tracking on the pairing adapter. |
| `gatt:service-changed` | gatt.service-changed | Service-changed arrives only where the OS surfaces it (CoreBluetooth); Windows/Linux need an adapter. |
| `gatt:maximum-write-length` | gatt.maximum-write-length | Measured MTU is wired into the core maximum-write-length on every characteristic/descriptor write (fail-closed when unmeasured); a dedicated maximumWriteLength host query needs an adapter. |
| `discovery:system-chooser` | chooser.system | Desktop has no system chooser; explicit selection needs an OS picker. |

## preapproved-limitation-candidate (8)

| Capability | Scenario | Bound |
|---|---|---|
| `discovery:advertisement-watch` | scan.observation-delivery | `os-delivery-bounded`: advertisement facts depend on OS delivery; masked fields stay absent. |
| `peer:origin-authorized` | peer.origin-authorized | `shell-owned-auth`: caller authentication is owned by the host shell (Tauri/Node), not the radio layer. |
| `gatt:long-write` | gatt.long-write | `no-prepared-write-path`: rejected up front, never silently single-written. |
| `gatt:reliable-write` | gatt.reliable-write | `no-atomic-execute-path`: rejected explicitly per OS availability. |
| `gatt:write-without-response-readiness` | gatt.write-readiness | `no-readiness-signal`: fire-and-forget within OS queue bounds. |
| `gatt:high-throughput-acquire` | gatt.high-throughput | `bounded-sequential-only`: no burst negotiation. |
| `background:desktop-maintain-connection` | background.desktop-maintain | `process-lifetime-only`: the OS keeps the link only while the host process lives. |
| `lifecycle:page-persistence` | lifecycle.page-persistence | `shell-owned-lifecycle`: reload/lease lifecycle across documents is owned by the host shell. |

## Per-OS verdicts (PR210 decision 7)

The tables above are the base verdicts. Where an operating system answers
differently, its own verdict replaces the base one at registration
(`DesktopCapability::per_os`, projected by `register_desktop_capabilities`
for the OS the crate is compiled for). `os-adapter-provides` rows are
implemented by the narrow adapters in `src/os/` behind `RadioBoundary`;
`os-adapter-compile-verified` means the OS I/O is type-checked
(`cargo check --target x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
`aarch64-pc-windows-msvc`) and its translation rules unit-tested
(`os::bluez_model`, `os::winrt_model`), with no physical-radio evidence.
`crates/ubm-desktop/tests/legacy_parity.rs` asserts, per OS, that every
capability the legacy CoreBluetooth / WinRT / BlueZ backend registered is
available here (provenance per row).

| Capability | OS | Verdict | Limitation | Implementation / reason |
|---|---|---|---|---|
| `connection:rssi` | windows | unsupported | — | btleplug's WinRT `read_rssi` is the last advertisement's RSSI, not a link measurement; legacy WinRT had none. |
| `connection:rssi` | linux | unsupported | — | btleplug's BlueZ `read_rssi` is the discovery-time `Device1.RSSI`; BlueZ has no connected RSSI; legacy BlueZ had none. |
| `security:state` | linux | os-adapter-provides | os-adapter-compile-verified | `Device1.Paired`/`Bonded` (Bonded wins where BlueZ has it); `pairingPossible` reported unknown (BlueZ has no such fact). |
| `security:state` | windows | os-adapter-provides | os-adapter-compile-verified | `DeviceInformationPairing.IsPaired`/`CanPair`. |
| `security:pair` | linux | os-adapter-provides | os-adapter-compile-verified | `Device1.Pair` through a just-works `Agent1` (`NoInputNoOutput`), registered only when pairing is requested. |
| `security:pair` | windows | os-adapter-provides | os-adapter-compile-verified | `PairAsync`, statuses mapped as the legacy addon did. |
| `security:cancel-pairing` | linux | os-adapter-provides | os-adapter-compile-verified | `Device1.CancelPairing`, sent only while this connection's `Pair` is in flight and no bond exists (BlueZ removes an existing bond on a `CancelPairing` with nothing in flight). |
| `security:cancel-pairing` | windows | os-adapter-provides | os-adapter-compile-verified | Cancels the in-flight `PairAsync` operation. |
| `security:unpair` | linux | os-adapter-provides | os-adapter-compile-verified | `Adapter1.RemoveDevice`. |
| `security:unpair` | windows | os-adapter-provides | os-adapter-compile-verified | `UnpairAsync`. |
| `security:pairing-generation` | linux | os-adapter-provides | host-supplied-controller | Registered only through `register_desktop_capabilities_with_pairing_generation`; the pair request carries the host's privileged `PairingGenerationController`, and the previous generation is restored (a failed restore is counted). |
| `peer:address-targeting` | linux | os-adapter-provides | os-adapter-compile-verified | Existing device object, else `Adapter1.ConnectDevice`, else (experimental method missing) an LE discovery session until the object exists. |
| `gatt:maximum-write-length` | windows | btleplug-provides | deterministic-only | Commands: btleplug's MTU (`GattSession.MaxPduSize`, 23 until the first change) - 3; requests and descriptor writes: a whole attribute value (512), because `WriteValueAsync` performs the long write, as the legacy addon and Tauri 4.x relied on (finding 81, `WriteLimits::os_long_write`). |
| `gatt:maximum-write-length` | linux | os-adapter-provides | os-adapter-compile-verified | Commands: `GattCharacteristic1.MTU` - 3; when BlueZ withholds the MTU, no gate below the 512-byte attribute value (the legacy BlueZ backend let BlueZ answer, finding 97); requests: a whole attribute value (BlueZ performs the long write). |
| `gatt:maximum-write-length` | macos | os-adapter-provides | deterministic-only | Vendored btleplug patch 1 (`vendor/btleplug/UBM_PATCHES.md`): `maximumWriteValueLengthForType:` per write type. Unsupported in a workspace linking crates.io btleplug. |
| `gatt:service-changed` | macos | btleplug-provides | deterministic-only | btleplug reports `didModifyServices`. |
| `gatt:service-changed` | linux | os-adapter-provides | os-adapter-compile-verified | `Device1.ServicesResolved` dropping under a live link. |
| `gatt:service-changed` | windows | os-adapter-provides | os-adapter-compile-verified | `BluetoothLEDevice.GattServicesChanged` on the maintained connection. |
| `gatt:write-without-response-readiness` | macos | os-adapter-provides | deterministic-only | Vendored btleplug patch 4: `write_readiness` (`canSendWriteWithoutResponse`) and `write_readiness_events` (`peripheralIsReadyToSendWriteWithoutResponse`), the legacy readiness watch. Unsupported in a workspace linking crates.io btleplug: without the patch no readiness signal exists. |
| `background:desktop-maintain-connection` | windows | os-adapter-provides | process-lifetime-only | `GattSession.MaintainConnection(true)` held per connection, released with it. |

Not capability rows, also closed here: adapter power read as a fact on
Linux (`Adapter1.Powered`; btleplug reports `PoweredOff` when its read
fails), `CBManager.authorization` on macOS (`adapter_authorization`),
per-OS adapter listing/selection (`list_adapters`: `hci0`… on Linux, the
native adapter id on Windows; an unnamed open with several adapters is
`adapter.ambiguous`), characteristic flags beyond the core bits
(`DiscoveredPath::access`: BlueZ `Flags`, elsewhere btleplug's property
byte), address type (`address_type`), and bond-change events
(`security_events`). The btleplug BlueZ `mtu()` unwrap panic is never
reached: Linux reads the MTU through the BlueZ adapter.

Delivery mode (finding 39): `plan_delivery` answers before any effect —
one property is certain, a requirement the characteristic lacks is
`gatt.property-not-supported`, and with both properties the platform rule
decides (CoreBluetooth and BlueZ enable notifications — Apple
`setNotifyValue(_:for:)`, BlueZ `src/shared/gatt-client.c`; Windows
rewrites the CCCD itself, notify preferred as legacy WinRT did, a hard
requirement honoured).

Discovery (findings 95, 96): the snapshot is in discovery (handle) order,
re-sorted from btleplug's UUID-first sets, with per-UUID occurrences in
that order. It registers whole or the discovery fails — a malformed OS UUID
is `protocol.malformed`, a database past the ATT handle space
`capability.limited` — as the legacy backends failed; no entry is skipped.
The core's bounds are protocol limits (65535 paths and subscriptions per
peer database, 3840 links) and a 65536-peer memory bound, never quotas
below legacy.

Admission and buffering (findings 106, 107): no bound sits below the legacy
backends. One lease may run any number of operations on each of its links
(the kernel bounds live operations at 65,536; legacy queued 8 per connection
with no per-owner cap). A subscription is shared by up to 65,536 consumers.
Each consumer's buffer, the scan-observation queue and the notification
ingress hold up to the public stream maximum (65,536 items; 4 MiB per
consumer, 16 MiB ingress). btleplug's event broadcasts hold 4,096 (patch 13).
Loss past any of these is reported, never silent.

Subscribe on BlueZ (finding 98): a characteristic that declares neither
notify nor indicate is handed to `StartNotify` and BlueZ answers, as the
legacy BlueZ backend did; CoreBluetooth and WinRT keep their legacy
property check. A hard delivery requirement is checked on every OS.

Error identity (finding 113): a platform failure carries its answer as
typed fields (`DesktopError::platform()`): CoreBluetooth `NSError` code,
WinRT `gatt-status`/`hresult`, BlueZ D-Bus error name (vendored patch 15),
and a BlueZ D-Bus failure keeps the legacy `platform.failure` identity.
CoreBluetooth read/notify provenance (finding 110, 5.0 departure from the
legacy addon): a read on a notifying characteristic runs, reads of one
characteristic complete in request order, and each read reports the radio's
provenance (`read-response`, or `read-or-notification` while the
characteristic can notify); the value still reaches subscribers (vendored
patch 14). WinRT and BlueZ always report `read-response`. Values a consumer holds when
its subscription is invalidated drain before the invalidation (finding 111).

Scan observations (findings 120–122): every OS sighting is an observation
(vendored patch 17). CoreBluetooth and WinRT report each advertisement with
its own data; BlueZ reports its merged `Device1` state on discovery and once per
`Device1` property-change signal, a name- or alias-only change included
(patch 18). On every platform, each known device is re-observed once as
`device-state` when a scan starts (finding 205; the legacy BlueZ backend did
this on Linux only), and CoreBluetooth sightings carry the advertised name, not
a GAP name read over a connection. Each observation is labelled
`advertisement` or `device-state`. Sightings are observations only while a
scan runs and belong to that scan (`take_scan_observation`: scan id and age);
a new scan starts from an empty queue. Tauri keeps its 4.x cadence: every
known peripheral is re-observed every 2 s during a scan
(`set_known_peer_refresh`). Unreadable sightings are counted and logged.
Pairing without a caller budget has no backstop (finding 123). Unsubscribe
failures carry the platform answer, and every BlueZ failure takes the legacy
`platform.failure` + `bluez-dbus` identity (finding 124).

Disconnect and delivery (findings 127–131):
- **Reconnect without a scan.** A disconnected peripheral stays known, and
  one the OS forgot is resolved again by identifier or address (vendored
  patch 19), so a reconnect by id works without a new scan, as in 4.x.
- **Pending work is answered at disconnect.** A connect, a discovery and
  every characteristic and descriptor operation in flight fail at once, as
  the legacy `failPendingForDevice` did. On BlueZ, a disconnect ends a
  pending service discovery.
- **Stream before enable.** The notification value stream is open before
  notifications are enabled, so the first values are not lost.
- **Values before a disconnect.** At a disconnect or service change,
  each forwarder is drained first: its queued values are delivered, or
  counted as loss, before the invalidation. The drain is bounded (1 s); a
  forwarder that overruns it is aborted and counted.
- **Ingress refusals are attributed.** A value the bounded ingress refuses
  is reported to its own subscription as upstream loss. The consumer's
  overflow policy applies: `error` ends the stream with an `overflow`
  terminal; a lossy policy counts the loss and stays live
  (`subscribe_with_policy`, `consumer_counters`). The radio-wide total is
  `ResourceCounters::ingress_notification_drops`.

Event wakes (finding 118): the central signals its observer (the napi
event waker) for security changes, write readiness and OS-ended scans, as it
does for values, lifecycle and adapter events, so the host takes every report
when it happens, as the legacy callbacks delivered them.

Adapter reads (finding 94): an adapter-state or authorization read is never
ended by an adapter reset; it answers the post-transition state, as every
legacy host did.

Read and write property flags (finding 83) are reported facts, not gates:
every legacy host let the OS answer a read or write whatever the flags
said, so the core admits it and the radio's own answer is the result. The
public GATT layer keeps its legacy write-mode resolution
(`src/public/gatt.ts` `resolveWriteMode`). Subscribe keeps its
notify-or-indicate check, which legacy had.

Also closed (round 4, LEGACY-AUDIT-1 findings 57-63, 65, 68):

- **Adapter loss (57).** On the production radio
  (`tears_down_on_adapter_loss`), a usable adapter that reports powered-off,
  resetting, unsupported or unauthorized, a refused authorization, a BlueZ
  `org.bluez` owner change or `Adapter1` removal (`os::linux` adapter
  watch), or a WinRT adapter device removal (`os::windows` adapter watch)
  tears down everything live: the core settles every live operation
  `Reset` and its waiting driver answers `operation.reset`; the owned scan
  ends aborted; every link publishes `LifecycleKind::AdapterLost` (legacy
  reason `'adapter'`); every subscription invalidates
  (`InvalidationCause::AdapterReset`); the OS scan stop and link releases
  are asked for (bounded, failures named); the attachment moves to new
  attachment, backend and adapter generations; one `AdapterResetEvent`
  reports it (`adapter_reset_events`). Legacy: `corebluetooth-backend.ts`
  `startAdapterLossCleanup`, `winrt-backend.ts` `handleAdapterState`,
  `bluez-backend-runtime.ts` `advanceBackendGeneration`.
- **Admission (58).** Operations refuse before any effect from the adapter
  facts the radio reported, in each OS's legacy order
  (`AdmissionPolicy::CoreBluetooth` = `assertCoreBluetoothOperational`,
  `AdmissionPolicy::WinRt` = `assertWinRtAdapterReady`, BlueZ none):
  `permission.denied` / `restricted` / `not-determined` (CoreBluetooth),
  `adapter.unavailable`, `adapter.powered-off`, `adapter.resetting`. A fact
  never reported admits. macOS re-reads `CBManager.authorization` on every
  state change so a cached pending decision never outlives the answer.
- **First usable state (59).** `open_btleplug` on macOS waits at most 10 s
  (legacy bound) for CoreBluetooth's first state and for a usable adapter
  (`await_usable_adapter`); past it the open fails `capability.unavailable`,
  detail `adapter-initialization-timed-out`. The CoreBluetooth scan start
  refuses a manager that is not powered on (vendored patch 8).
- **Adapter states (60).** `AdapterPowerState` adds `resetting`,
  `unsupported`, `unauthorized`; `adapter_status()` reports availability
  (`available` / `unavailable` after removal / `unsupported` / `unknown`).
- **Repeated UUIDs (61).** Vendored patch 6 keeps every same-UUID service,
  characteristic and descriptor on all three OSes, and notifications carry
  the instance: same-UUID instances subscribe side by side.
- **WinRT discovery (62).** Vendored patch 9: uncached at every level, no
  cached fallback, failures surfaced with the status, the table replaced on
  every discovery.
- **Scan duplicates and transport (63).** Vendored patch 8 and
  `start_scan_with(.., ScanDuplicatePolicy, ..)`.
- **Connection-level maximum write length (65).**
  `connection_maximum_write_length(peer, lease, with_response, ..)` needs no
  discovery.
- **WinRT adapter selection (68).** A non-default adapter opens by id: its
  power, state changes and authorization are that adapter's while LE
  traffic goes through the Windows stack, as the legacy addon did
  (`winrt-boundary.inc` `SelectAdapter`). `AdapterListing.deployment` and
  `host_deployment()` carry the legacy `deployment` diagnostic.

- **Broadcast loss (PR210-78).** Vendored patch 10: notifications the OS
  broadcast lost before a subscription read them end its `error`-policy
  stream with an `overflow` terminal counting them; lost adapter events are
  counted (`radio_events_lost`). Nothing is skipped silently.

Evidence: macOS unit and scriptable-radio tests (`tests/adapter_loss.rs`,
`tests/notification_loss.rs`,
vendored CoreBluetooth patch tests with real CoreBluetooth attribute
objects); Windows and Linux type-checked only; no physical radio run.

Also closed (round 3): WinRT scan-terminated (`scan_terminal_events`,
vendored patch 5: the watcher's own `Stopped`, requested stops excluded;
the OS event source closing is reported the same way on every platform),
and the upstream WinRT defect that re-registered the advertisement
handler on every scan start (duplicated advertisements after the first
scan). Descriptor write mode is at parity: the legacy WinRT addon accepted
only with-response descriptor writes, which is all this path offers.

Also closed (round 2): macOS advertisement solicited / overflow service
UUIDs and the connectable flag (`PeerSnapshot::extras`, vendored btleplug
patch 2; `None` where the platform does not report them — legacy WinRT and
BlueZ did not); Windows adapter authorization
(`DeviceAccessInformation.CurrentStatus`, the legacy addon's mapping);
BlueZ D-Bus bus choice (`CentralProfile::bluez_bus`, legacy `busKind`:
the btleplug manager and the BlueZ adapter both use the chosen bus through
vendored patch 3; a bus the build cannot honour fails the open with
`capability.unsupported`, never falls back to the system bus).

## Counts

36 required desktop rows: 6 btleplug-provides, 22 narrow-OS-adapter-needed,
8 preapproved-limitation-candidate. Zero rows closed by re-description; zero
rows removed. The physical qualification slice (available-macOS/Windows/Linux
read/notify/cleanup on real radios) remains the open boundary after this
slice.

## Operation control, release retention and lifecycle (PR210)

Not capability rows; recorded here because they bound every row above.

- Every `DesktopCentral` operation takes an `OpControl` (caller budget +
  cancellation ticket). The caller budget is the only deadline; the named
  liveness backstops (`LIVENESS_OP` 120 s, `LIVENESS_CLEANUP` 10 s,
  `LIVENESS_SCAN_START` 30 s) apply only without one and report
  `operation.timed-out` with detail `liveness-backstop`. A connect has no
  backstop (finding 112): without a budget it waits as long as the OS does,
  as the legacy pending CoreBluetooth connect and Android `autoConnect` did,
  and a cancel ends it. btleplug exposes no
  radio abort: a cancelled or expired call is dropped, and the OS may still
  finish it — a dispatched write therefore reports commit `unknown`,
  retryability `never`.
- A failed, hung or cancelled release (scan stop, unsubscribe, disconnect)
  keeps the resource; the next attempt calls the OS again. A timed-out
  disconnect stays `Disconnecting` (no post-timeout link probe); the OS
  finishing it later publishes `Released { requested: true }` and a retry
  answers `AlreadyReleased`.
- Link loss, confirmed release, service changes and adapter power-state
  changes are published as typed events (`lifecycle_events`,
  `adapter_events`, profile observer). Adapter loss publishes a reset
  (below).
- GATT state is cached per connected peer in `BtleplugRadio`; services are
  rediscovered once per connection/service change, not per verb.
- The drop-safety tests are hardware-gated (`#[ignore]`, run with
  `cargo test -p ubm-desktop --test radio_drop_safety -- --ignored`); an
  ignored run is not physical evidence.
