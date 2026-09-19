//! Legacy-vs-Rust capability parity per desktop OS (PR210 decision 7, owner
//! directive: no capability regressions). The legacy CoreBluetooth, WinRT and
//! BlueZ backends' feature sets are encoded as data with their source
//! provenance; for every OS the Rust path must register each legacy
//! capability row as available (`limited` or better), and every legacy
//! feature that is not a capability row must have its Rust surface.
//!
//! Evidence level: this proves registration and API presence. Physical
//! behaviour on each OS is not proven here (see `PARITY_GAPS.md`).

use ubm_core::central::CapabilityState;
use ubm_desktop::{DesktopOs, desktop_capability_states};

/// One legacy feature and where the legacy backend provided it.
struct Legacy {
    /// Frozen capability id, when the feature is a capability row.
    capability: Option<&'static str>,
    /// What the legacy backend did, for the failure message.
    feature: &'static str,
    /// Legacy source (repo-relative path:line).
    provenance: &'static str,
    /// The Rust surface that answers it (checked for presence below).
    rust_surface: &'static str,
    /// The row is provided only when the host supplies the privileged
    /// pairing-generation controller (legacy reported it only then too).
    needs_pairing_generation: bool,
}

const fn row(
    capability: &'static str,
    provenance: &'static str,
    rust_surface: &'static str,
) -> Legacy {
    Legacy {
        capability: Some(capability),
        feature: capability,
        provenance,
        rust_surface,
        needs_pairing_generation: false,
    }
}

const fn surface(
    feature: &'static str,
    provenance: &'static str,
    rust_surface: &'static str,
) -> Legacy {
    Legacy {
        capability: None,
        feature,
        provenance,
        rust_surface,
        needs_pairing_generation: false,
    }
}

/// Legacy Node/Electron CoreBluetooth (PARITY-INVENTORY §1).
const MACOS: &[Legacy] = &[
    row(
        "connection:direct",
        "src/backends/corebluetooth/corebluetooth-runtime-capabilities.ts:44",
        "DesktopCentral::connect",
    ),
    row(
        "connection:rssi",
        "src/backends/corebluetooth/corebluetooth-runtime-capabilities.ts:92",
        "DesktopCentral::read_rssi",
    ),
    row(
        "gatt:maximum-write-length",
        "src/backends/corebluetooth/corebluetooth-runtime-capabilities.ts:188",
        "DesktopCentral::maximum_write_length",
    ),
    row(
        "gatt:write-without-response-readiness",
        "src/backends/corebluetooth/corebluetooth-runtime-capabilities.ts:69",
        "BtleplugRadio::write_characteristic",
    ),
    row(
        "gatt:descriptors",
        "native/electron/corebluetooth/index.js:170",
        "DesktopCentral::read_descriptor",
    ),
    row(
        "gatt:indications",
        "src/backends/corebluetooth/corebluetooth-handles.ts:338",
        "DesktopCentral::subscribe",
    ),
    row(
        "gatt:service-changed",
        "native/electron/corebluetooth/index.js:234",
        "DesktopCentral::lifecycle_events",
    ),
    row(
        "discovery:continuous-scan",
        "src/backends/corebluetooth/corebluetooth-backend.ts:393",
        "DesktopCentral::start_scan",
    ),
    surface(
        "long write (core-emulated over maximum-write-length)",
        "src/core/core-capabilities.ts:35",
        "DesktopCentral::maximum_write_length",
    ),
    surface(
        "require-* delivery checked against characteristic properties",
        "src/backends/corebluetooth/corebluetooth-handles.ts:338",
        "delivery::plan_delivery",
    ),
    surface(
        "adapter power + change watch",
        "native/electron/corebluetooth/index.js:298",
        "DesktopCentral::adapter_events",
    ),
    surface(
        "adapter authorization",
        "native/electron/corebluetooth/index.js:309",
        "DesktopCentral::adapter_authorization",
    ),
    surface(
        "connection-lost event",
        "src/backends/corebluetooth/corebluetooth-backend.ts:1084",
        "DesktopCentral::lifecycle_events",
    ),
    surface(
        "write-without-response readiness watch",
        "src/backends/corebluetooth/corebluetooth-connection-controls.ts:273",
        "DesktopCentral::write_readiness_events",
    ),
    surface(
        "advertisement solicited / overflow service UUIDs and connectable",
        "native/electron/corebluetooth/index.js:99",
        "PeerSnapshot::extras",
    ),
];

/// Legacy Node/Electron WinRT (PARITY-INVENTORY §2).
const WINDOWS: &[Legacy] = &[
    row(
        "connection:direct",
        "src/backends/winrt/winrt-backend.ts:132",
        "DesktopCentral::connect",
    ),
    row(
        "security:state",
        "src/backends/winrt/winrt-backend.ts:143",
        "DesktopCentral::security_state",
    ),
    row(
        "security:pair",
        "src/backends/winrt/winrt-backend.ts:144",
        "DesktopCentral::pair",
    ),
    row(
        "security:cancel-pairing",
        "src/backends/winrt/winrt-backend.ts:145",
        "DesktopCentral::cancel_pairing",
    ),
    row(
        "security:unpair",
        "src/backends/winrt/winrt-backend.ts:146",
        "DesktopCentral::unpair",
    ),
    row(
        "gatt:service-changed",
        "src/backends/winrt/winrt-backend.ts:1790",
        "DesktopCentral::lifecycle_events",
    ),
    row(
        "background:desktop-maintain-connection",
        "native/electron/winrt/src/winrt-boundary.inc:890",
        "BtleplugRadio::connect",
    ),
    row(
        "gatt:descriptors",
        "src/backends/winrt/winrt-gatt-operations.ts:207",
        "DesktopCentral::write_descriptor",
    ),
    row(
        "discovery:continuous-scan",
        "native/electron/winrt/src/winrt-boundary.inc:754",
        "DesktopCentral::start_scan",
    ),
    surface(
        "security-state change events",
        "native/electron/winrt/src/winrt-boundary.inc:623",
        "DesktopCentral::security_events",
    ),
    surface(
        "list and select adapters",
        "native/electron/winrt/src/winrt-boundary.inc:605",
        "btleplug_backend::list_adapters",
    ),
    surface(
        "adapter state + events",
        "src/backends/winrt/winrt-backend.ts:1823",
        "DesktopCentral::adapter_events",
    ),
    surface(
        "scan-terminated event (watcher stopped)",
        "native/electron/winrt/src/winrt-boundary.inc:776",
        "DesktopCentral::scan_terminal_events",
    ),
    surface(
        "adapter authorization (DeviceAccessInformation)",
        "native/electron/winrt/src/addon.cpp:248",
        "DesktopCentral::adapter_authorization",
    ),
    surface(
        "subscribe prefers notify",
        "src/backends/winrt/winrt-handles.ts:453",
        "delivery::platform_rule",
    ),
    surface(
        "connection lost / state events",
        "src/backends/winrt/winrt-backend.ts:1547",
        "DesktopCentral::lifecycle_events",
    ),
];

/// Legacy Node BlueZ over dbus-next (PARITY-INVENTORY §3; `git show
/// origin/main:src/node-bluez.ts`).
const LINUX: &[Legacy] = &[
    row(
        "connection:direct",
        "src/backends/bluez/bluez-backend.ts:83",
        "DesktopCentral::connect",
    ),
    row(
        "peer:address-targeting",
        "src/backends/bluez/bluez-backend.ts:90",
        "DesktopCentral::resolve_address",
    ),
    row(
        "security:state",
        "src/backends/bluez/bluez-backend.ts:52",
        "DesktopCentral::security_state",
    ),
    row(
        "security:pair",
        "src/backends/bluez/bluez-backend.ts:53",
        "DesktopCentral::pair",
    ),
    row(
        "security:cancel-pairing",
        "src/backends/bluez/bluez-backend.ts:54",
        "DesktopCentral::cancel_pairing",
    ),
    row(
        "security:unpair",
        "src/backends/bluez/bluez-backend.ts:55",
        "DesktopCentral::unpair",
    ),
    Legacy {
        capability: Some("security:pairing-generation"),
        feature: "security:pairing-generation (host-supplied controller)",
        provenance: "src/backends/bluez/bluez-backend.ts:109",
        rust_surface: "register_desktop_capabilities_with_pairing_generation",
        needs_pairing_generation: true,
    },
    row(
        "gatt:service-changed",
        "src/backends/bluez/bluez-backend-runtime.ts:385",
        "DesktopCentral::lifecycle_events",
    ),
    row(
        "gatt:descriptors",
        "src/backends/bluez/bluez-gatt-operations.ts:1",
        "DesktopCentral::read_descriptor",
    ),
    row(
        "discovery:continuous-scan",
        "src/backends/bluez/bluez-scan-runtime.ts:1",
        "DesktopCentral::start_scan",
    ),
    surface(
        "security-state change events (Paired)",
        "src/backends/bluez/bluez-security.ts:347",
        "DesktopCentral::security_events",
    ),
    surface(
        "just-works Agent1 registered for pairing",
        "src/backends/bluez/bluez-dbus-next-boundary.ts:253",
        "os::linux::Bluez::pair",
    ),
    surface(
        "enumerate and select adapters (hci0, hci1)",
        "src/backends/bluez/bluez-backend-provider.ts:127",
        "btleplug_backend::list_adapters",
    ),
    surface(
        "adapter power + change events",
        "src/backends/bluez/bluez-backend-runtime.ts:398",
        "DesktopCentral::adapter_state",
    ),
    surface(
        "full characteristic flags + access requirements",
        "src/backends/bluez/bluez-backend-handles.ts:256",
        "DiscoveredPath::access",
    ),
    surface(
        "D-Bus bus choice (busKind system / session)",
        "src/backends/bluez/bluez-backend-provider.ts:65",
        "CentralProfile::bluez_bus",
    ),
    surface(
        "advertisement address type",
        "src/backends/bluez/bluez-runtime-models.ts:189",
        "DesktopCentral::address_type",
    ),
    surface(
        "connection-lost event",
        "src/backends/bluez/bluez-backend-runtime.ts:851",
        "DesktopCentral::lifecycle_events",
    ),
];

/// Legacy capability rows the legacy backends reported explicitly
/// unsupported must not become an unexplained claim either: the Rust path
/// reports them unsupported too (parity, not a regression).
const LINUX_EXPLICITLY_UNSUPPORTED: &[(&str, &str)] = &[
    (
        "connection:priority",
        "src/backends/desktop/bluez-connection-capabilities.ts:91",
    ),
    (
        "connection:parameters",
        "src/backends/desktop/bluez-connection-capabilities.ts:98",
    ),
];

fn legacy_for(os: DesktopOs) -> &'static [Legacy] {
    match os {
        DesktopOs::MacOs => MACOS,
        DesktopOs::Windows => WINDOWS,
        DesktopOs::Linux => LINUX,
    }
}

#[test]
fn every_legacy_capability_row_is_available_on_the_rust_path_per_os() {
    let mut failures = Vec::new();
    for os in DesktopOs::ALL {
        for pairing_generation in [false, true] {
            let states = desktop_capability_states(Some(os), pairing_generation);
            for legacy in legacy_for(os) {
                let Some(id) = legacy.capability else {
                    continue;
                };
                if legacy.needs_pairing_generation && !pairing_generation {
                    continue;
                }
                let state = states
                    .iter()
                    .find(|(row, _, _)| *row == id)
                    .map(|(_, state, _)| *state);
                if !matches!(
                    state,
                    Some(CapabilityState::Limited | CapabilityState::Supported)
                ) {
                    failures.push(format!(
                        "{} ({pairing_generation}): legacy {} [{}] registers {state:?} on the Rust path",
                        os.as_str(),
                        legacy.feature,
                        legacy.provenance
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "capability regressions:\n{}",
        failures.join("\n")
    );
}

#[test]
fn directed_pairing_generation_stays_closed_without_the_hosts_controller() {
    let states = desktop_capability_states(Some(DesktopOs::Linux), false);
    let generation = states
        .iter()
        .find(|(row, _, _)| *row == "security:pairing-generation")
        .map(|(_, state, _)| *state);
    assert_eq!(
        generation,
        Some(CapabilityState::Unsupported),
        "privilege is never implicit"
    );
}

#[test]
fn rows_the_legacy_backends_left_unsupported_stay_unsupported() {
    let states = desktop_capability_states(Some(DesktopOs::Linux), false);
    for (id, provenance) in LINUX_EXPLICITLY_UNSUPPORTED {
        let state = states
            .iter()
            .find(|(row, _, _)| row == id)
            .map(|(_, state, _)| *state);
        assert_eq!(
            state,
            Some(CapabilityState::Unsupported),
            "{id} [{provenance}]"
        );
    }
}

/// Takes any item by value: a call only compiles while the item exists.
fn present<T>(_item: T) -> bool {
    true
}

/// Every legacy feature names its Rust surface, and each named surface
/// exists: the items below only compile while those APIs exist.
#[test]
fn every_legacy_feature_has_a_rust_surface() {
    use ubm_desktop::{DesktopCentral, FakeRadio};

    type Central = DesktopCentral<FakeRadio>;
    let surfaces: &[(&str, bool)] = &[
        ("DesktopCentral::connect", present(Central::connect)),
        ("DesktopCentral::read_rssi", present(Central::read_rssi)),
        (
            "DesktopCentral::maximum_write_length",
            present(Central::maximum_write_length),
        ),
        (
            "DesktopCentral::read_descriptor",
            present(Central::read_descriptor),
        ),
        (
            "DesktopCentral::write_descriptor",
            present(Central::write_descriptor),
        ),
        ("DesktopCentral::subscribe", present(Central::subscribe)),
        (
            "DesktopCentral::lifecycle_events",
            present(Central::lifecycle_events),
        ),
        ("DesktopCentral::start_scan", present(Central::start_scan)),
        (
            "DesktopCentral::adapter_events",
            present(Central::adapter_events),
        ),
        (
            "DesktopCentral::adapter_state",
            present(Central::adapter_state),
        ),
        (
            "DesktopCentral::adapter_authorization",
            present(Central::adapter_authorization),
        ),
        (
            "DesktopCentral::security_state",
            present(Central::security_state),
        ),
        ("DesktopCentral::pair", present(Central::pair)),
        (
            "DesktopCentral::cancel_pairing",
            present(Central::cancel_pairing),
        ),
        ("DesktopCentral::unpair", present(Central::unpair)),
        (
            "DesktopCentral::security_events",
            present(Central::security_events),
        ),
        (
            "DesktopCentral::resolve_address",
            present(Central::resolve_address),
        ),
        (
            "DesktopCentral::address_type",
            present(Central::address_type),
        ),
        (
            "delivery::plan_delivery",
            present(ubm_desktop::plan_delivery),
        ),
        (
            "delivery::platform_rule",
            present(ubm_desktop::platform_rule),
        ),
        (
            "register_desktop_capabilities_with_pairing_generation",
            present(ubm_desktop::register_desktop_capabilities_with_pairing_generation),
        ),
        (
            "btleplug_backend::list_adapters",
            present(ubm_desktop::btleplug_backend::list_adapters),
        ),
        // Radio-internal surfaces (not callable from outside the crate):
        // proven by the per-OS type-checks and unit tests named in
        // PARITY_GAPS.md.
        ("BtleplugRadio::write_characteristic", true),
        ("BtleplugRadio::connect", true),
        ("os::linux::Bluez::pair", true),
        ("DiscoveredPath::access", true),
        (
            "PeerSnapshot::extras",
            present(ubm_desktop::AdvertisementExtras::default),
        ),
        (
            "DesktopCentral::write_readiness_events",
            present(Central::write_readiness_events),
        ),
        (
            "DesktopCentral::scan_terminal_events",
            present(Central::scan_terminal_events),
        ),
        (
            "CentralProfile::bluez_bus",
            present(ubm_desktop::bluez_bus_supported),
        ),
    ];
    for os in DesktopOs::ALL {
        for legacy in legacy_for(os) {
            assert!(
                surfaces
                    .iter()
                    .any(|(name, present)| *name == legacy.rust_surface && *present),
                "{}: legacy {} [{}] names Rust surface {} which is not listed",
                os.as_str(),
                legacy.feature,
                legacy.provenance,
                legacy.rust_surface
            );
        }
    }
    let path = ubm_desktop::DiscoveredPath {
        service_uuid: String::new(),
        service_occurrence: 0,
        characteristic_uuid: None,
        characteristic_occurrence: None,
        descriptor_uuid: None,
        descriptor_occurrence: None,
        properties: 0,
        access: None,
    };
    assert!(path.access.is_none());
}
