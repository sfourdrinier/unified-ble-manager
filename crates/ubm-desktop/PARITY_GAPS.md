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
| `connection:rssi` | connection.rssi-and-att-mtu-capability-contract | RSSI reported only when the OS measures it (`deterministic-only`); absent values stay absent, never synthesized. |
| `gatt:descriptors` | gatt.descriptor-discovery-read-write | Descriptor discovery/reads/writes with occurrence identity (`deterministic-only`); direct CCCD writes fail `gatt.cccd-managed`, sharing stays with subscribe/unsubscribe. |
| `gatt:indications` | gatt.indications | Subscribed values buffer per consumer and are observable through the take API (`delivery-kind-unknown`: the btleplug stream does not distinguish indications from notifications). Per-instance routing by (service, occurrence, characteristic, occurrence) filtered on the full (service, characteristic) identity; same-scope duplicate instances are rejected at enable time (`gatt.subscribe-failed`, ambiguous routing) because the peripheral-wide OS stream carries no occurrence identity — bytes are never fanned out to invented instances. |

## narrow-OS-adapter-needed (22)

| Capability | Scenario | Missing adapter |
|---|---|---|
| `scan:platform-options` | scan.platform-options | Active/passive/PHY scan knobs; btleplug exposes service-UUID filters only. |
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

## Counts

36 required desktop rows: 6 btleplug-provides, 22 narrow-OS-adapter-needed,
8 preapproved-limitation-candidate. Zero rows closed by re-description; zero
rows removed. The physical qualification slice (available-macOS/Windows/Linux
read/notify/cleanup on real radios) remains the open boundary after this
slice.
