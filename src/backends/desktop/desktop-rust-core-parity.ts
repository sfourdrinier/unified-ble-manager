// src/backends/desktop/desktop-rust-core-parity.ts
//
// Legacy-vs-Rust capability parity for the desktop hosts (FIX-PLAN decision
// 7/11, PARITY-INVENTORY §1–3). Every capability the legacy CoreBluetooth,
// WinRT and dbus-next BlueZ backends provided has one row. A row is either
// `implemented` on the Rust path, or `blocked` on a named core/OS-adapter
// method that has not landed — never "unsupported" for something that used
// to work.
//
// This table is a verification artifact, not a default gate: the desktop
// factories execute the Rust core unconditionally. Each `implemented` row is
// proven by a test (desktop-rust-core-provider.test.js). A row that regresses
// to `blocked` must name the missing method and owner and gain a FAILING
// probe in __tests__/backends/desktop/desktop-parity-blockers.test.js; blocked
// rows block release.

import type { DesktopRustCorePlatform } from './desktop-rust-core-binding'

export interface DesktopRustCoreParityRow {
  readonly id: string
  readonly platforms: readonly DesktopRustCorePlatform[]
  /** What the legacy backend did. */
  readonly legacy: string
  /** `implemented`, or the missing method and the packet that owns it. */
  readonly rust:
    | { readonly state: 'implemented'; readonly how: string }
    | {
        readonly state: 'blocked'
        readonly missing: string
        readonly owner: 'DESKTOP-PARITY' | 'CORE' | 'DESKTOP-FACTORIES'
      }
}

const ALL: readonly DesktopRustCorePlatform[] = Object.freeze(['bluez', 'corebluetooth', 'winrt'])

function implemented(how: string): DesktopRustCoreParityRow['rust'] {
  return Object.freeze({ state: 'implemented', how })
}

function row(
  id: string,
  platforms: readonly DesktopRustCorePlatform[],
  legacy: string,
  rust: DesktopRustCoreParityRow['rust']
): DesktopRustCoreParityRow {
  return Object.freeze({ id, platforms: Object.freeze([...platforms]), legacy, rust })
}

export const DESKTOP_RUST_CORE_PARITY: readonly DesktopRustCoreParityRow[] = Object.freeze([
  row(
    'scan.os-service-filter',
    ALL,
    'scanner.plan + trustedServiceUuidFilter pushed service UUIDs to the OS',
    implemented('scanner.plan; planned service UUIDs cross to DesktopCentral::start_scan')
  ),
  row(
    'scan.os-name-prefix',
    ['bluez'],
    'a caller localNamePrefix became the SetDiscoveryFilter Pattern',
    implemented('localNamePrefix crosses to DesktopCentral::start_scan_matching; btleplug sends it as Pattern')
  ),
  row(
    'scan.software-filters',
    ALL,
    'name-prefix / manufacturer / address predicates matched in software',
    implemented('advertisementMatchesFilter per consumer')
  ),
  row(
    'scan.share-join',
    ALL,
    'one native scan fanned out to joined leases',
    implemented('TS fan-out over one core scan')
  ),
  row(
    'scan.duplicate-first',
    ['bluez'],
    'DuplicateData=false delivered first sightings',
    implemented('per-consumer first-sighting filter')
  ),
  row(
    'connection.lost-event',
    ALL,
    'connection-lost backend event',
    implemented('UbmCentral.takeLifecycleEvent (link-lost)')
  ),
  row(
    'gatt.database-changed-event',
    ALL,
    'database-changed backend event',
    implemented('UbmCentral.takeLifecycleEvent (services-changed)')
  ),
  row(
    'adapter.power-and-watch',
    ALL,
    'adapter power state + change events',
    implemented('UbmCentral.adapterState + takeAdapterEvent')
  ),
  row(
    'adapter.enumerate-select',
    ['bluez', 'winrt'],
    'list and select among adapters',
    implemented('UbmCentral.listAdapters + open({ adapterId })')
  ),
  row(
    'operation.cancel-in-flight',
    ALL,
    'dispatcher cancelled any in-flight operation',
    implemented('tickets: createTicket/cancelTicket -> DesktopCentral::cancel')
  ),
  row(
    'gatt.write-without-response-commit',
    ALL,
    'without-response writes',
    implemented("commitState 'unknown' (never 'confirmed')")
  ),
  row(
    'gatt.require-delivery-property-check',
    ALL,
    'require-* checked against characteristic properties',
    implemented('TS property check before dispatch (decision 3)')
  ),
  row(
    'connection.rssi',
    ['corebluetooth'],
    'CBPeripheral.readRSSI (connection:rssi limited)',
    implemented('UbmCentral.readRssi -> DesktopCentral::read_rssi')
  ),
  row(
    'connection.effective-mtu',
    ALL,
    'OS-measured ATT MTU (connection:effective-mtu limited): macOS maximumWriteValueLength(.withResponse) + 3 (finding 217, same as Apple RN), Windows GattSession.MaxPduSize, Linux BlueZ characteristic MTU',
    implemented('UbmCentral.readEffectiveMtu -> DesktopCentral::read_effective_mtu')
  ),
  row(
    'connection.priority-parameters-reasons',
    ['bluez'],
    'priority/parameters registered unsupported with reasons',
    implemented('createBluezConnectionControlRegistrations')
  ),
  row(
    'gatt.delivery-requirement-to-core',
    ALL,
    'the requested CCCD mode reached the platform (Tauri 4.x property check; WinRT notify preference)',
    implemented(
      'WinRT carries require-* to DesktopCentral::subscribe (os::windows writes the CCCD); CoreBluetooth and BlueZ keep the legacy property check (decision 3); delivery is what the core observed'
    )
  ),
  row(
    'gatt.maximum-write-length',
    ['corebluetooth'],
    'maximumWriteValueLength(for:) (gatt:maximum-write-length limited; long write follows)',
    implemented(
      'UbmCentral.maximumWriteLength -> DesktopCentral::maximum_write_length (macOS OS adapter); long write derives from it'
    )
  ),
  row(
    'gatt.write-without-response-readiness',
    ['corebluetooth'],
    'canSendWriteWithoutResponse + peripheralIsReady readiness watch',
    implemented('connections.writeWithoutResponseReadiness -> DesktopCentral::write_readiness + write_readiness_events')
  ),
  row(
    'adapter.authorization',
    ['corebluetooth'],
    'CBManager.authorization (denied / not-determined / granted)',
    implemented('UbmCentral.adapterAuthorization -> DesktopCentral::adapter_authorization (also Windows)')
  ),
  row(
    'scan.advertisement-extended-fields',
    ['corebluetooth'],
    'solicited / overflow service UUIDs and connectable',
    implemented('PeerSnapshot.extras (patched btleplug on macOS) -> observation fields')
  ),
  row(
    'security.pair-unpair-state',
    ['winrt', 'bluez'],
    'security state, pair (BlueZ just-works Agent1), cancel pairing, unpair, security events',
    implemented(
      'UbmCentral security ops -> DesktopCentral::{security_state,pair,cancel_pairing,unpair,security_events}'
    )
  ),
  row(
    'security.pairing-generation',
    ['bluez'],
    'host-supplied privileged pairing-generation controller',
    implemented(
      'UbmCentral.installPairingGenerationController (napi ThreadsafeFunction) -> PairRequest.generation_controller; the core holds and restores the generation'
    )
  ),
  row(
    'connection.maintain',
    ['winrt'],
    'GattSession.MaintainConnection(true) held per lease',
    implemented(
      'os::windows holds MaintainConnection per connection; background:desktop-maintain-connection registered'
    )
  ),
  row(
    'gatt.notify-preference',
    ['winrt'],
    'notify chosen over indicate when both are present',
    implemented(
      'os::windows rewrites the CCCD to notify when no indication is required (delivery::plan_delivery AdapterSelects)'
    )
  ),
  row(
    'gatt.descriptor-write-mode',
    ['winrt'],
    'descriptor writes accepted only with-response ("Windows GATT descriptors do not support write-without-response")',
    implemented(
      'the same with-response-only rule, refused before dispatch (native/electron/winrt/src/winrt-boundary.inc:1181)'
    )
  ),
  row(
    'scan.terminated-event',
    ['winrt'],
    'watcher stop surfaced as a scan terminal (OnScanTerminal)',
    implemented('DesktopCentral::scan_terminal_events -> consumers end source-failed (aborted) or closed')
  ),
  row(
    'peer.address-targeting',
    ['bluez'],
    'peerFromAddress + Adapter1.ConnectDevice + address-filtered scan',
    implemented(
      'connections.peerFromAddress -> DesktopCentral::resolve_address at connect; address predicates in software'
    )
  ),
  row(
    'adapter.dbus-session-bus',
    ['bluez'],
    'busKind session selected the D-Bus session bus',
    implemented(
      'busKind -> UbmCentral.open({ bluezBus }) -> CentralProfile.bluez_bus; listing via list_adapters_on(bus) (session bus needs the vendored bluez-async build; physical check under dbus-run-session pending)'
    )
  ),
  row(
    'scan.address-type',
    ['bluez'],
    'advertisement address type public / random',
    implemented('DesktopCentral::address_type, asked once per addressed peer')
  ),
  row(
    'gatt.characteristic-flags',
    ['bluez'],
    'broadcast / signed-write / extended / reliable-write / auxiliaries flags + encrypt/authenticate access',
    implemented('DiscoveredPath.access -> characteristic properties (availability known) and access requirements')
  ),
  row(
    'adapter.loss-teardown',
    ALL,
    'adapter loss (power-off, reset, removal, bluetoothd restart) cancelled in-flight operations, ended scans and subscriptions source-failed, ended links (CoreBluetooth/WinRT: connection-state-changed reason adapter; BlueZ: invalidated), advanced the backend generation and cleared peer ids (CoreBluetooth/BlueZ: backend-restarted)',
    implemented(
      'DesktopCentral tears down on adapter loss (tickets settle operation.reset, aborted scan terminal, adapter-lost lifecycle, adapter-reset invalidation); UbmCentral.takeAdapterResetEvent -> TS advances the generation and applies ADAPTER_LOSS_SEQUENCE per OS'
    )
  ),
  row(
    'adapter.admission-errors',
    ALL,
    'radio work refused before any effect: permission.denied / restricted / not-determined, adapter.unavailable, adapter.powered-off, adapter.resetting',
    implemented(
      'DesktopCentral precheck under the radio admission_policy (CoreBluetooth / WinRT legacy ordering; BlueZ lifecycle only, as legacy)'
    )
  ),
  row(
    'adapter.first-state-wait',
    ['corebluetooth'],
    'waited up to 10 s for the first usable CoreBluetooth state; capability.unavailable / adapter-initialization-timed-out otherwise',
    implemented(
      'UbmCentral.awaitUsableAdapter(10 s) on every CoreBluetooth open; the timeout maps to the legacy capability.unavailable / adapter-initialization-timed-out'
    )
  ),
  row(
    'adapter.power-resetting-unsupported',
    ALL,
    'adapter power resetting and availability unsupported / unavailable reported as the OS said',
    implemented('AdapterPowerState resetting / unsupported / unauthorized -> the legacy CoreBluetooth snapshot mapping')
  ),
  row(
    'gatt.repeated-uuid-occurrences',
    ALL,
    'services, characteristics and descriptors sharing a UUID kept separate occurrences',
    implemented(
      'vendored btleplug patch attribute-instances (all three OSes); the core registers each instance by occurrence'
    )
  ),
  row(
    'gatt.winrt-uncached-discovery',
    ['winrt'],
    'discovery always uncached, failures reported, rediscovery after GattServicesChanged',
    implemented(
      'vendored btleplug patch winrt-uncached-discovery: uncached reads, failures surfaced, cache refreshed on GattServicesChanged'
    )
  ),
  row(
    'scan.os-duplicate-and-transport',
    ALL,
    'BlueZ Transport le + DuplicateData from the policy; CoreBluetooth AllowDuplicates NO',
    implemented(
      'vendored btleplug patch scan-policy: BlueZ Transport le, DuplicateData / AllowDuplicates from the policy'
    )
  ),
  row(
    'scan.duplicate-merged',
    ALL,
    "duplicatePolicy 'merged' accepted (the default of scanForServices / scanUntil)",
    implemented('accepted; the policy crosses to DesktopCentral::start_scan_with, which sets the OS duplicate filter')
  ),
  row(
    'gatt.maximum-write-length-before-discovery',
    ['corebluetooth'],
    'connection.maximumWriteLength(mode) answered without a discovered database',
    implemented(
      'UbmCentral.connectionMaximumWriteLength -> DesktopCentral::connection_maximum_write_length (no selector, no discovery)'
    )
  ),
  row(
    'capability.unsupported-reasons',
    ['corebluetooth', 'bluez'],
    'unsupported rows kept their limitations: CoreBluetooth request-mtu / phy (effective-mtu is limited where the core wires the OS derivation); BlueZ pairing-generation privilege and adapter-wide blast radius',
    implemented(
      'createCoreBluetoothUnsupportedRegistrations + createBluezPairingGenerationRegistration (privilege note without a controller, adapter-wide note with one)'
    )
  ),
  row(
    'identity.legacy-ids',
    ALL,
    'backend ids unified-ble:corebluetooth / winrt / bluez-dbus; adapter ids corebluetooth-default-adapter, /org/bluez/hciN, raw WinRT ids; *_BACKEND_ID exports',
    implemented(
      'DESKTOP_RUST_CORE_PROFILES carry the legacy backend/provider ids; desktopRustCoreAdapterId maps the OS label to the legacy adapter id; *_BACKEND_ID re-exported'
    )
  ),
  row(
    'adapter.winrt-select-and-deployment',
    ['winrt'],
    'SelectAdapter -> FromIdAsync for a non-default adapter; deployment (packaged / unpackaged) in diagnostics and adapter limitations',
    implemented(
      'vendored btleplug patch winrt-adapter-by-id; AdapterListing.deployment -> adapter limitation + diagnostics.deployment'
    )
  )
])
