//! Desktop central capability parity table (HOST-DESKTOP).
//!
//! Every required desktop central capability maps to exactly one verdict:
//! [`CapabilityVerdict::BtleplugProvides`], [`CapabilityVerdict::NarrowOsAdapterNeeded`],
//! or [`CapabilityVerdict::LimitationCandidate`]. Gaps stay open —
//! re-describing a gap never closes it — and the full mapping ships as the
//! committed `PARITY_GAPS.md` report. [`register_desktop_capabilities`]
//! projects the same table into the real core as capability descriptors so
//! the runtime truth matches the report.

use ubm_core::central::{CapabilityDescriptor, CapabilityState, Central, EvidenceLevel};
use ubm_core::contracts::CoreError;

/// Parity verdict for one required desktop central capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityVerdict {
    /// Implemented through a supported btleplug path in this crate.
    /// Reported as `limited` until physical-radio evidence qualifies it.
    BtleplugProvides,
    /// Needs a narrow per-OS adapter on top of btleplug. Open work.
    NarrowOsAdapterNeeded,
    /// Preapproved-limitation candidate: explicitly bounded behavior with a
    /// named limitation, never silent downgrade. The gap stays open in the
    /// report until the limitation is approved.
    LimitationCandidate,
    /// Implemented by a narrow OS adapter in this crate (`crate::os`) on the
    /// platforms a row names. Reported `limited` with the evidence limit it
    /// carries until physical-radio evidence qualifies it.
    OsAdapterProvides,
}

impl CapabilityVerdict {
    /// Frozen verdict string for the report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BtleplugProvides => "btleplug-provides",
            Self::NarrowOsAdapterNeeded => "narrow-OS-adapter-needed",
            Self::LimitationCandidate => "preapproved-limitation-candidate",
            Self::OsAdapterProvides => "os-adapter-provides",
        }
    }
}

/// A desktop operating system with its own verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesktopOs {
    MacOs,
    Windows,
    Linux,
}

impl DesktopOs {
    /// The OS this crate is compiled for, when it is a desktop OS.
    #[must_use]
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }

    /// Frozen label for the report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }

    /// Every desktop OS.
    pub const ALL: [Self; 3] = [Self::MacOs, Self::Windows, Self::Linux];
}

/// Evidence limit of an OS-adapter row whose OS I/O is only type-checked
/// from the qualification host (its translation rules are unit-tested).
const OS_ADAPTER_COMPILE_VERIFIED: &str = "os-adapter-compile-verified";

/// One platform's own verdict for a row, replacing the row's base verdict
/// on that platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsOverride {
    pub os: DesktopOs,
    pub verdict: CapabilityVerdict,
    /// Limitation code for `limited` descriptors (`None` for unsupported).
    pub limitation: Option<&'static str>,
    /// Report note: what provides it on this OS, or why it cannot.
    pub note: &'static str,
    /// The row is provided only when the host supplied a privileged
    /// pairing-generation controller (see
    /// [`register_desktop_capabilities_with_pairing_generation`]).
    pub needs_pairing_generation_controller: bool,
}

impl OsOverride {
    /// A row an OS adapter in this crate provides on `os`.
    #[must_use]
    pub const fn adapter(os: DesktopOs, note: &'static str) -> Self {
        Self {
            os,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some(OS_ADAPTER_COMPILE_VERIFIED),
            note,
            needs_pairing_generation_controller: false,
        }
    }

    /// A row this OS cannot provide, with the reason.
    #[must_use]
    pub const fn unsupported(os: DesktopOs, note: &'static str) -> Self {
        Self {
            os,
            verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
            limitation: None,
            note,
            needs_pairing_generation_controller: false,
        }
    }
}

/// macOS `gatt:maximum-write-length`: provided when btleplug carries the
/// vendored CoreBluetooth write-length patch (`vendor/btleplug`,
/// `UBM_PATCHES.md`), detected at build time; without it btleplug's
/// CoreBluetooth `mtu()` never leaves 23 and nothing measured exists.
#[cfg(btleplug_ubm_write_length)]
const MACOS_WRITE_LENGTH_VERDICT: CapabilityVerdict = CapabilityVerdict::OsAdapterProvides;
#[cfg(btleplug_ubm_write_length)]
const MACOS_WRITE_LENGTH_LIMITATION: Option<&str> = Some("deterministic-only");
#[cfg(btleplug_ubm_write_length)]
const MACOS_WRITE_LENGTH_NOTE: &str = "Vendored btleplug patch: CBPeripheral.maximumWriteValueLength(for:) per write type, read on connect.";
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_WRITE_LENGTH_VERDICT: CapabilityVerdict = CapabilityVerdict::NarrowOsAdapterNeeded;
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_WRITE_LENGTH_LIMITATION: Option<&str> = None;
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_WRITE_LENGTH_NOTE: &str = "Unpatched btleplug 0.12: CoreBluetooth mtu() stays 23, so no measured write length exists; the vendored patch (vendor/btleplug) provides it.";

/// macOS `connection:effective-mtu`: the route exists and answers
/// `unavailable`. CoreBluetooth does not observe an ATT MTU.
/// `maximumWriteValueLength` stays a per-mode write capacity.
#[cfg(btleplug_ubm_write_length)]
const MACOS_EFFECTIVE_MTU_VERDICT: CapabilityVerdict = CapabilityVerdict::OsAdapterProvides;
#[cfg(btleplug_ubm_write_length)]
const MACOS_EFFECTIVE_MTU_LIMITATION: Option<&str> = Some("corebluetooth-att-mtu-not-observed");
#[cfg(btleplug_ubm_write_length)]
const MACOS_EFFECTIVE_MTU_NOTE: &str = "CoreBluetooth does not observe an ATT MTU. maximumWriteValueLength(for:) remains the per-mode write capacity and is not reported as an ATT MTU. The observation is unavailable.";
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_EFFECTIVE_MTU_VERDICT: CapabilityVerdict = CapabilityVerdict::OsAdapterProvides;
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_EFFECTIVE_MTU_LIMITATION: Option<&str> = Some("corebluetooth-att-mtu-not-observed");
#[cfg(not(btleplug_ubm_write_length))]
const MACOS_EFFECTIVE_MTU_NOTE: &str = "CoreBluetooth does not observe an ATT MTU. The observation is unavailable; a write-length API is not a measurement of one ATT PDU.";

/// macOS `gatt:write-without-response-readiness`: with the vendored patch
/// (UBM_PATCHES.md #4) the legacy readiness watch exists
/// (`DesktopCentral::write_readiness` + `write_readiness_events`); without
/// it no readiness signal exists at all — `write_without_response_ready`
/// answers `capability.unsupported` — so the row stays open work (F11),
/// never a claim that btleplug provides it.
#[cfg(btleplug_ubm_write_readiness)]
const MACOS_READINESS_VERDICT: CapabilityVerdict = CapabilityVerdict::OsAdapterProvides;
#[cfg(btleplug_ubm_write_readiness)]
const MACOS_READINESS_LIMITATION: Option<&str> = Some("deterministic-only");
#[cfg(not(btleplug_ubm_write_readiness))]
const MACOS_READINESS_VERDICT: CapabilityVerdict = CapabilityVerdict::NarrowOsAdapterNeeded;
#[cfg(not(btleplug_ubm_write_readiness))]
const MACOS_READINESS_LIMITATION: Option<&str> = None;

/// One required desktop central capability and its parity verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopCapability {
    /// Frozen capability id.
    pub id: &'static str,
    /// Parity verdict.
    pub verdict: CapabilityVerdict,
    /// Primary acceptance scenario.
    pub scenario: &'static str,
    /// Report note: what provides it, what adapter is missing, or which
    /// limitation bounds it.
    pub note: &'static str,
    /// Limitation code for `limited` descriptors (`None` for unsupported).
    pub limitation: Option<&'static str>,
    /// Platforms whose own verdict replaces the base verdict.
    pub per_os: &'static [OsOverride],
}

impl DesktopCapability {
    /// The verdict, limitation and note on `os` (`None`: a non-desktop
    /// build, which keeps the base verdicts). `pairing_generation` says
    /// whether the host supplied a pairing-generation controller.
    #[must_use]
    pub fn on(
        &self,
        os: Option<DesktopOs>,
        pairing_generation: bool,
    ) -> (CapabilityVerdict, Option<&'static str>, &'static str) {
        let found = os.and_then(|os| self.per_os.iter().find(|entry| entry.os == os));
        match found {
            Some(entry) if entry.needs_pairing_generation_controller && !pairing_generation => {
                (CapabilityVerdict::NarrowOsAdapterNeeded, None, entry.note)
            }
            Some(entry) => (entry.verdict, entry.limitation, entry.note),
            None => (self.verdict, self.limitation, self.note),
        }
    }
}

/// Required desktop central capabilities: the frozen desktop matrix rows
/// this host must answer. Mobile-only roles (Apple restoration,
/// Android service) are out of the desktop scope and noted in
/// `PARITY_GAPS.md`, not carried as desktop rows.
pub const DESKTOP_CAPABILITIES: &[DesktopCapability] = &[
    DesktopCapability {
        id: "discovery:continuous-scan",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "scan.owner-join-authority-and-signature",
        note: "One global scan owner with explicit stop and event-source-close cleanup.",
        limitation: Some("one-global-scan-owner"),
        per_os: &[],
    },
    DesktopCapability {
        id: "discovery:advertisement-watch",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "scan.observation-delivery",
        note: "Advertisement facts depend on OS delivery (privacy-masked addresses/fields stay absent, never synthesized).",
        limitation: Some("os-delivery-bounded"),
        per_os: &[],
    },
    DesktopCapability {
        id: "scan:platform-options",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "scan.platform-options",
        note: "Per-OS scan controls. WinRT defaults active, with extended advertisements disabled; BlueZ narrows by name prefix.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Windows,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("winrt-versioned-scan-options"),
            note: "Active/passive and versioned None reception; optional extended advertisement reception requires runtime API and default-adapter support. The watcher is system-wide, not adapter-targeted.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "peer:resolve-reference",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "peer.resolve-reference",
        note: "Platform-guid resolution for observed peers; address domains need OS identity adapters.",
        limitation: Some("platform-guid-only"),
        per_os: &[],
    },
    DesktopCapability {
        id: "peer:address-targeting",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.address-targeting",
        note: "CoreBluetooth hides addresses entirely; address targeting needs a narrow OS identity adapter.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ (os::linux): an existing device object by address, otherwise an owned LE discovery session until the object exists. Address resolution never calls Adapter1.ConnectDevice or establishes a link.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT typed Bluetooth address lookup validates both address and public/random type, retained for subsequent connection acquisition; lookup does not establish a link.",
            ),
        ],
    },
    DesktopCapability {
        id: "peer:known",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.known-peers",
        note: "OS-known peer lookup is platform-specific; CoreBluetooth resolves explicit identifiers, not an unrestricted OS peer inventory.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::MacOs,
                "Read-only retrievePeripheralsWithIdentifiers on the existing manager; explicit identifiers only, no connection ownership.",
            ),
            OsOverride::adapter(
                DesktopOs::Linux,
                "Daemon-owner-fenced selected-adapter Device1 cache, including unbonded records; OS visibility only, no local lease.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "OS-visible LE DeviceInformation selector on the default adapter; cached/paired visibility only, no local lease.",
            ),
        ],
    },
    DesktopCapability {
        id: "peer:system-connected",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.system-connected",
        note: "System-connected retrieval does not acquire a local connection lease.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::MacOs,
                "Read-only retrieveConnectedPeripheralsWithServices; nonempty service filters required; local connection state is independent.",
            ),
            OsOverride::adapter(
                DesktopOs::Linux,
                "Selected-adapter Device1 inventory with positive org.bluez.Bearer.LE1 connected evidence; aggregate Classic connectivity is not substituted.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "Read-only connected LE selector and current connection status on the default adapter; service filters unavailable, no local connection ownership.",
            ),
        ],
    },
    DesktopCapability {
        id: "peer:bonded",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.bonded",
        note: "The bond store is OS-side; reading it needs a narrow adapter.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Windows,
                "Read-only paired BluetoothLE DeviceInformation inventory, validated against current pairing state; no connection lease is acquired.",
            ),
            OsOverride::adapter(
                DesktopOs::Linux,
                "Read-only selected-adapter Device1 inventory from the bound BlueZ owner, using explicit Paired/Bonded facts; no connection lease is acquired.",
            ),
        ],
    },
    DesktopCapability {
        id: "peer:origin-authorized",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "peer.origin-authorized",
        note: "Caller authentication is owned by the host shell (Tauri/Node), not the radio layer.",
        limitation: Some("shell-owned-auth"),
        per_os: &[],
    },
    DesktopCapability {
        id: "peer:restored",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "peer.restored",
        note: "Desktop processes start without restored BLE handles; adopting OS-restored peers needs a narrow restoration adapter per platform.",
        limitation: None,
        per_os: &[],
    },
    DesktopCapability {
        id: "connection:direct",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "connection.lease-joins-borrowing-transfer-and-revocation",
        note: "Direct connection and ownership cleanup are implemented; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
        per_os: &[],
    },
    DesktopCapability {
        id: "connection:when-available",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.when-available",
        note: "Deferred auto-connect needs an OS adapter/daemon path beyond btleplug connect.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "The maintained daemon's revision-1 LE availability observer reports fresh connectable LE advertisements; an independently owned LE discovery session waits before existing sender-scoped lease acquisition. This is scan-triggered initial acquisition, not autonomous reconnect.",
            ),
            OsOverride::adapter(
                DesktopOs::MacOs,
                "CoreBluetooth pending connect waits for the known peer's availability; actual native callback settles acquisition, caller cancellation releases pending ownership.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "GattSession MaintainConnection is acquired before discovery and native ConnectionStatusChanged reports availability; caller cancellation releases the owned maintained session and callback.",
            ),
        ],
    },
    DesktopCapability {
        id: "connection:rssi",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "connection.rssi-and-att-mtu-capability-contract",
        note: "RSSI reported only when the OS measures the link (CoreBluetooth readRSSI); Windows and Linux report advertisement RSSI only and answer unsupported; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
        per_os: &[
            OsOverride::unsupported(
                DesktopOs::Windows,
                "btleplug 0.12's WinRT read_rssi answers the last advertisement's RSSI (winrtble/peripheral.rs last_rssi), not a link measurement; the legacy WinRT backend had no connected RSSI either.",
            ),
            OsOverride::unsupported(
                DesktopOs::Linux,
                "btleplug 0.12's BlueZ read_rssi answers the discovery-time Device1.RSSI (bluez/peripheral.rs read_rssi), not a link measurement; BlueZ has no connected-RSSI API and the legacy BlueZ backend had none.",
            ),
        ],
    },
    DesktopCapability {
        id: "connection:effective-mtu",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.rssi-and-att-mtu-capability-contract",
        note: "The OS-measured MTU already feeds every write through the core maximum-write-length (fail-closed when unmeasured); exposing the negotiated value as a host read is answered per OS below.",
        limitation: None,
        per_os: &[
            OsOverride {
                os: DesktopOs::MacOs,
                verdict: MACOS_EFFECTIVE_MTU_VERDICT,
                limitation: MACOS_EFFECTIVE_MTU_LIMITATION,
                note: MACOS_EFFECTIVE_MTU_NOTE,
                needs_pairing_generation_controller: false,
            },
            OsOverride {
                os: DesktopOs::Windows,
                verdict: CapabilityVerdict::BtleplugProvides,
                limitation: Some("winrt-gattsession-max-pdu-size"),
                note: "btleplug 0.12's WinRT mtu() is the GattSession.MaxPduSize it tracks from MaxPduSizeChanged (winrtble/ble/device.rs), reported as the ATT MTU; deterministic-only until physical-radio qualification.",
                needs_pairing_generation_controller: false,
            },
            OsOverride {
                os: DesktopOs::Linux,
                verdict: CapabilityVerdict::OsAdapterProvides,
                limitation: Some("bluez-gatt-characteristic-mtu"),
                note: "BlueZ (os::linux): the org.bluez.GattCharacteristic1 MTU of the link's characteristics; a link BlueZ withholds it on answers capability.unavailable, never a guessed 23. Deterministic-only until physical-radio qualification.",
                needs_pairing_generation_controller: false,
            },
        ],
    },
    DesktopCapability {
        id: "connection:request-mtu",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.mtu-request",
        note: "MTU request is an OS control path (Android requestMtu / Apple negotiated) needing an adapter.",
        limitation: None,
        per_os: &[],
    },
    DesktopCapability {
        id: "connection:priority",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.priority",
        note: "Connection-priority control needs a per-OS adapter.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Windows,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("winrt-preferred-parameters-22000"),
            note: "WinRT preferred presets; acceptance is not an observed outcome. The connection owns the request until replacement or teardown. Runtime API presence is required.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "connection:parameters",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.parameters",
        note: "Connection parameters need a per-OS observation. BlueZ and CoreBluetooth do not expose this read.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Windows,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("winrt-connection-parameters-22000"),
            note: "WinRT BluetoothLEDevice.GetConnectionParameters and ConnectionParametersChanged. Present from Windows 11 build 22000; an older OS answers capability.unavailable with winrt-connection-parameters-requires-windows-11-22000. Values are observed, not a preferred-parameter request.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "connection:phy",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.phy",
        note: "PHY observation needs a per-OS adapter; selection is separate.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Windows,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("winrt-phy-read-only-22000"),
            note: "WinRT GetConnectionPhy reports measured TX/RX on Windows 11 build 22000+. PHY selection is unsupported.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "connection:subrate",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "connection.subrate",
        note: "Subrate control needs a per-OS adapter.",
        limitation: None,
        per_os: &[],
    },
    DesktopCapability {
        id: "security:state",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.state",
        note: "Observed link-security readout needs a per-OS adapter; never inferred from a flag.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ over D-Bus (os::linux): Device1.Paired/Bonded, Device1.Pair with a just-works NoInputNoOutput Agent1 registered only when pairing is requested, CancelPairing only while this connection's Pair is in flight, Adapter1.RemoveDevice; bond changes are watched.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT DeviceInformationPairing (os::windows): IsPaired/CanPair, PairAsync cancelled through its IAsyncOperation, UnpairAsync; state published after pair/unpair like the legacy addon.",
            ),
        ],
    },
    DesktopCapability {
        id: "security:pair",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.pair",
        note: "Pairing ceremony is OS-mediated and needs a narrow adapter.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ over D-Bus (os::linux): Device1.Paired/Bonded, Device1.Pair with a just-works NoInputNoOutput Agent1 registered only when pairing is requested, CancelPairing only while this connection's Pair is in flight, Adapter1.RemoveDevice; bond changes are watched.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT DeviceInformationPairing (os::windows): IsPaired/CanPair, PairAsync cancelled through its IAsyncOperation, UnpairAsync; state published after pair/unpair like the legacy addon.",
            ),
        ],
    },
    DesktopCapability {
        id: "security:cancel-pairing",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.cancel-pairing",
        note: "Pairing cancellation needs the same OS adapter as pairing.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ over D-Bus (os::linux): Device1.Paired/Bonded, Device1.Pair with a just-works NoInputNoOutput Agent1 registered only when pairing is requested, CancelPairing only while this connection's Pair is in flight, Adapter1.RemoveDevice; bond changes are watched.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT DeviceInformationPairing (os::windows): IsPaired/CanPair, PairAsync cancelled through its IAsyncOperation, UnpairAsync; state published after pair/unpair like the legacy addon.",
            ),
        ],
    },
    DesktopCapability {
        id: "security:unpair",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.unpair",
        note: "Unpair/remove-bond needs a per-OS adapter.",
        limitation: None,
        per_os: &[
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ over D-Bus (os::linux): Device1.Paired/Bonded, Device1.Pair with a just-works NoInputNoOutput Agent1 registered only when pairing is requested, CancelPairing only while this connection's Pair is in flight, Adapter1.RemoveDevice; bond changes are watched.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT DeviceInformationPairing (os::windows): IsPaired/CanPair, PairAsync cancelled through its IAsyncOperation, UnpairAsync; state published after pair/unpair like the legacy addon.",
            ),
        ],
    },
    DesktopCapability {
        id: "security:custom-ceremony",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.custom-ceremony",
        note: "No cryptographic handshake is invented here; custom ceremony needs a reviewed profile plus adapter.",
        limitation: None,
        per_os: &[],
    },
    DesktopCapability {
        id: "security:pairing-generation",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "security.pairing-generation",
        note: "Bond-generation tracking rides the pairing adapter.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Linux,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("host-supplied-controller"),
            note: "The adapter-level generation hold runs only with a host-supplied privileged PairingGenerationController and a profile registered through register_desktop_capabilities_with_pairing_generation; without both, directed generations fail closed.",
            needs_pairing_generation_controller: true,
        }],
    },
    DesktopCapability {
        id: "gatt:descriptors",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "gatt.descriptor-discovery-read-write",
        note: "Descriptor discovery, reads, and writes are implemented; deterministic-only until radio qualification.",
        limitation: Some("deterministic-only"),
        per_os: &[],
    },
    DesktopCapability {
        id: "gatt:indications",
        verdict: CapabilityVerdict::BtleplugProvides,
        scenario: "gatt.indications",
        note: "Subscribed notification values buffer per consumer and are observable through the take API; the btleplug stream does not distinguish indications from notifications, so per-value delivery kind is unknown, while the enable's CCCD mode is answered from the characteristic's properties and the platform rule (finding 39).",
        limitation: Some("delivery-kind-unknown"),
        per_os: &[],
    },
    DesktopCapability {
        id: "gatt:service-changed",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "gatt.service-changed",
        note: "Service-changed arrives only where the OS surfaces it (CoreBluetooth); Win/Linux need an adapter.",
        limitation: None,
        per_os: &[
            OsOverride {
                os: DesktopOs::MacOs,
                verdict: CapabilityVerdict::BtleplugProvides,
                limitation: Some("deterministic-only"),
                note: "btleplug 0.12 reports CoreBluetooth didModifyServices as DeviceServicesModified.",
                needs_pairing_generation_controller: false,
            },
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ (os::linux): Device1.ServicesResolved dropping under a live link, the legacy backend's database-changed source.",
            ),
            OsOverride::adapter(
                DesktopOs::Windows,
                "WinRT (os::windows): BluetoothLEDevice.GattServicesChanged on the maintained connection, as the legacy addon did.",
            ),
        ],
    },
    DesktopCapability {
        id: "gatt:maximum-write-length",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "gatt.maximum-write-length",
        note: "Measured MTU is wired into the core maximum-write-length on every characteristic/descriptor write (fail-closed when unmeasured); a dedicated maximumWriteLength host query needs an adapter.",
        limitation: None,
        per_os: &[
            OsOverride {
                os: DesktopOs::Windows,
                verdict: CapabilityVerdict::BtleplugProvides,
                limitation: Some("deterministic-only"),
                note: "btleplug 0.12's WinRT mtu() tracks GattSession.MaxPduSize (winrtble/ble/device.rs); measured limits admit OS-managed ordinary with-response writes up to 512 bytes, while without-response writes remain bounded by one ATT payload. Explicit prepared long-write transactions remain unsupported.",
                needs_pairing_generation_controller: false,
            },
            OsOverride::adapter(
                DesktopOs::Linux,
                "BlueZ (os::linux): GattCharacteristic1.MTU for commands; a request carries a whole attribute value because BlueZ WriteValue performs the long-write procedure (src/gatt-client.c).",
            ),
            OsOverride {
                os: DesktopOs::MacOs,
                verdict: MACOS_WRITE_LENGTH_VERDICT,
                limitation: MACOS_WRITE_LENGTH_LIMITATION,
                note: MACOS_WRITE_LENGTH_NOTE,
                needs_pairing_generation_controller: false,
            },
        ],
    },
    DesktopCapability {
        id: "gatt:long-write",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.long-write",
        note: "Prepared-write transactions have no btleplug path; long-write is rejected, never silently single-written.",
        limitation: Some("no-prepared-write-path"),
        per_os: &[],
    },
    DesktopCapability {
        id: "gatt:reliable-write",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.reliable-write",
        note: "Atomic execute has no btleplug path; rejected explicitly per OS availability.",
        limitation: Some("no-atomic-execute-path"),
        per_os: &[],
    },
    DesktopCapability {
        id: "gatt:write-without-response-readiness",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "gatt.write-readiness",
        note: "No readiness signal exists on this path; writes are fire-and-forget within OS queue bounds.",
        limitation: Some("no-readiness-signal"),
        per_os: &[OsOverride {
            os: DesktopOs::MacOs,
            verdict: MACOS_READINESS_VERDICT,
            limitation: MACOS_READINESS_LIMITATION,
            note: "Readiness probe (canSendWriteWithoutResponse) and readiness reports (peripheralIsReadyToSendWriteWithoutResponse) through vendored btleplug patch 4, which the row requires: without it no readiness signal exists and the probe answers capability.unsupported.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "gatt:high-throughput-acquire",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "gatt.high-throughput",
        note: "Explicit acquired packet transports require an OS mechanism; ordinary writes and notifications are not a substitute.",
        limitation: None,
        per_os: &[OsOverride {
            os: DesktopOs::Linux,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("optional-eligible-characteristic-acquired-fd"),
            note: "AcquireWrite/AcquireNotify are optional per characteristic. Native flags, method presence, conflicts and the returned MTU govern admission. Physical acquired-FD qualification remains separate.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "background:desktop-maintain-connection",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "background.desktop-maintain",
        note: "The OS keeps the link only while the host process lives; no execution promise beyond OS policy.",
        limitation: Some("process-lifetime-only"),
        per_os: &[OsOverride {
            os: DesktopOs::Windows,
            verdict: CapabilityVerdict::OsAdapterProvides,
            limitation: Some("process-lifetime-only"),
            note: "WinRT (os::windows): GattSession.MaintainConnection(true) held for each connection and released with it, as the legacy addon did; the OS keeps the link only while the process lives.",
            needs_pairing_generation_controller: false,
        }],
    },
    DesktopCapability {
        id: "lifecycle:page-persistence",
        verdict: CapabilityVerdict::LimitationCandidate,
        scenario: "lifecycle.page-persistence",
        note: "Page/lease lifecycle across reloads is owned by the host shell, not the radio layer.",
        limitation: Some("shell-owned-lifecycle"),
        per_os: &[],
    },
    DesktopCapability {
        id: "discovery:system-chooser",
        verdict: CapabilityVerdict::NarrowOsAdapterNeeded,
        scenario: "chooser.system",
        note: "Desktop has no system chooser; explicit user selection needs an OS picker adapter.",
        limitation: None,
        per_os: &[],
    },
];

/// Project the parity table into a live core for the OS this crate is
/// compiled for, so runtime capability truth matches the report row for
/// row. Provided rows register `limited` (deterministic evidence at most);
/// adapter-needed rows register `unsupported`; limitation candidates
/// register `limited` with their named limitation. `no-readiness-signal`
/// registers `unsupported` and keeps that limitation: Windows and Linux
/// have no write-without-response readiness signal. The host supplies no
/// pairing-generation controller.
pub fn register_desktop_capabilities(core: &mut Central) -> Result<(), CoreError> {
    register_for(core, DesktopOs::current(), false)
}

/// Same as [`register_desktop_capabilities`] for a host that supplies a
/// privileged [`crate::PairingGenerationController`] on its pair requests:
/// `security:pairing-generation` registers where an OS adapter can hold it
/// (Linux). Set it as `CentralProfile::register_capabilities` only when the
/// host will pass that controller.
pub fn register_desktop_capabilities_with_pairing_generation(
    core: &mut Central,
) -> Result<(), CoreError> {
    register_for(core, DesktopOs::current(), true)
}

/// The state and limitation each row registers on `os`.
#[must_use]
pub fn desktop_capability_states(
    os: Option<DesktopOs>,
    pairing_generation: bool,
) -> Vec<(&'static str, CapabilityState, Option<&'static str>)> {
    DESKTOP_CAPABILITIES
        .iter()
        .map(|capability| {
            let (verdict, limitation, _) = capability.on(os, pairing_generation);
            let state = if limitation == Some("no-readiness-signal") {
                CapabilityState::Unsupported
            } else {
                match verdict {
                    CapabilityVerdict::NarrowOsAdapterNeeded => CapabilityState::Unsupported,
                    _ => CapabilityState::Limited,
                }
            };
            (capability.id, state, limitation)
        })
        .collect()
}

/// Register the rows `os` answers (`None`: base verdicts) — for hosts that
/// simulate a platform's radio in hardware-free tests. Production hosts use
/// [`register_desktop_capabilities`] or
/// [`register_desktop_capabilities_with_pairing_generation`], which register
/// the OS this crate is compiled for.
pub fn register_desktop_capabilities_for(
    core: &mut Central,
    os: Option<DesktopOs>,
    pairing_generation: bool,
) -> Result<(), CoreError> {
    register_for(core, os, pairing_generation)
}

fn register_for(
    core: &mut Central,
    os: Option<DesktopOs>,
    pairing_generation: bool,
) -> Result<(), CoreError> {
    for capability in DESKTOP_CAPABILITIES {
        let (verdict, limitation, _) = capability.on(os, pairing_generation);
        let (state, evidence, limitations): (CapabilityState, EvidenceLevel, Vec<&str>) =
            if limitation == Some("no-readiness-signal") {
                (
                    CapabilityState::Unsupported,
                    EvidenceLevel::Blocked,
                    vec!["no-readiness-signal"],
                )
            } else {
                match verdict {
                    CapabilityVerdict::BtleplugProvides
                    | CapabilityVerdict::LimitationCandidate
                    | CapabilityVerdict::OsAdapterProvides => (
                        CapabilityState::Limited,
                        EvidenceLevel::Deterministic,
                        limitation.into_iter().collect(),
                    ),
                    CapabilityVerdict::NarrowOsAdapterNeeded => (
                        CapabilityState::Unsupported,
                        EvidenceLevel::Blocked,
                        Vec::from([NOT_IMPLEMENTED]),
                    ),
                }
            };
        core.register_capability(CapabilityDescriptor::new(
            capability.id,
            state,
            &[("availability", 1)],
            &limitations,
            &format!("ubm-desktop-capability-{}", capability.id),
            evidence,
            env!("CARGO_PKG_VERSION"),
            "ubm-desktop-capability-manifest-v1",
            &[capability.scenario],
        )?)?;
    }
    Ok(())
}

/// Limitation code for rows with no implementation yet.
const NOT_IMPLEMENTED: &str = "not-implemented";

/// Instance-specific reason for a scan-capable BlueZ radio whose pinned daemon
/// does not implement the versioned lifecycle/lease/GATT authority contract.
pub const BLUEZ_LE_AUTHORITY_REQUIRED: &str = "bluez-linux-authority-contract-required";

pub(crate) fn apply_connection_parameters_capability_limitation(
    core: &mut Central,
    reason: Option<&str>,
) -> Result<(), CoreError> {
    apply_control_capability_limitation(core, "connection:parameters", reason)
}

pub(crate) fn apply_control_capability_limitation(
    core: &mut Central,
    id: &str,
    reason: Option<&str>,
) -> Result<(), CoreError> {
    let Some(reason) = reason else {
        return Ok(());
    };
    let capability = DESKTOP_CAPABILITIES
        .iter()
        .find(|row| row.id == id)
        .expect("registered parameter capability");
    core.register_capability(CapabilityDescriptor::new(
        capability.id,
        CapabilityState::Unavailable,
        &[],
        &[reason],
        "ubm-desktop-instance-connection-parameters",
        EvidenceLevel::Blocked,
        env!("CARGO_PKG_VERSION"),
        "ubm-desktop-instance-capability-v1",
        &[capability.scenario],
    )?)?;
    Ok(())
}

pub(crate) fn apply_when_available_capability_limitation(
    core: &mut Central,
    reason: Option<(CapabilityState, &str)>,
) -> Result<(), CoreError> {
    let Some((state, reason)) = reason else {
        return Ok(());
    };
    let capability = DESKTOP_CAPABILITIES
        .iter()
        .find(|row| row.id == "connection:when-available")
        .expect("registered deferred connection row");
    core.register_capability(CapabilityDescriptor::new(
        capability.id,
        state,
        &[("availability", 0)],
        &[reason],
        "ubm-desktop-instance-when-available",
        EvidenceLevel::Blocked,
        env!("CARGO_PKG_VERSION"),
        "ubm-desktop-instance-capability-v1",
        &[capability.scenario],
    )?)?;
    Ok(())
}

/// Apply the instantiated radio's connection refusal, not a platform assumption.
/// Deterministic and other host radios keep their own registered capabilities.
pub(crate) fn apply_connection_capability_limitation(
    core: &mut Central,
    reason: Option<&str>,
) -> Result<(), CoreError> {
    let Some(reason) = reason else {
        return Ok(());
    };
    for capability in DESKTOP_CAPABILITIES.iter().filter(|row| {
        matches!(
            row.id,
            "connection:direct" | "background:desktop-maintain-connection"
        )
    }) {
        core.register_capability(CapabilityDescriptor::new(
            capability.id,
            CapabilityState::Unsupported,
            &[("availability", 0)],
            &[reason],
            &format!("ubm-desktop-instance-{}", capability.id),
            EvidenceLevel::Blocked,
            env!("CARGO_PKG_VERSION"),
            "ubm-desktop-instance-capability-v1",
            &[capability.scenario],
        )?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{DESKTOP_CAPABILITIES, register_desktop_capabilities};

    #[test]
    fn parameter_capability_uses_the_instantiated_api_refusal() {
        let mut core = test_core();
        super::register_desktop_capabilities_for(&mut core, Some(super::DesktopOs::Windows), false)
            .unwrap();
        super::apply_connection_parameters_capability_limitation(
            &mut core,
            Some("test-runtime-api-absent"),
        )
        .unwrap();
        let states = core.registered_capability_states();
        assert_eq!(
            states
                .iter()
                .find(|(id, _)| id == "connection:parameters")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unavailable
        );
        assert_ne!(
            states
                .iter()
                .find(|(id, _)| id == "connection:rssi")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unavailable
        );
        let mut supported = test_core();
        super::register_desktop_capabilities_for(
            &mut supported,
            Some(super::DesktopOs::Windows),
            false,
        )
        .unwrap();
        let before = supported.registered_capability_states();
        super::apply_connection_parameters_capability_limitation(&mut supported, None).unwrap();
        assert_eq!(supported.registered_capability_states(), before);
    }

    #[test]
    fn instance_le_availability_refusal_does_not_disable_direct_connection() {
        let mut core = test_core();
        super::register_desktop_capabilities_for(&mut core, Some(super::DesktopOs::Linux), false)
            .unwrap();
        super::apply_when_available_capability_limitation(
            &mut core,
            Some((
                ubm_core::central::CapabilityState::Unsupported,
                "native-observer-missing",
            )),
        )
        .unwrap();
        let states = core.registered_capability_states();
        assert_eq!(
            states
                .iter()
                .find(|(id, _)| id == "connection:when-available")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unsupported
        );
        assert_ne!(
            states
                .iter()
                .find(|(id, _)| id == "connection:direct")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unsupported
        );
        super::apply_when_available_capability_limitation(
            &mut core,
            Some((
                ubm_core::central::CapabilityState::Unavailable,
                "native-observer-probe-failed",
            )),
        )
        .unwrap();
        assert_eq!(
            core.registered_capability_states()
                .iter()
                .find(|(id, _)| id == "connection:when-available")
                .unwrap()
                .1,
            ubm_core::central::CapabilityState::Unavailable
        );
    }

    #[test]
    fn instance_connection_refusal_overrides_only_dependent_mechanisms() {
        use ubm_core::central::CapabilityState;
        for os in super::DesktopOs::ALL {
            let mut core = test_core();
            super::register_desktop_capabilities_for(&mut core, Some(os), false).unwrap();
            let before = core.registered_capability_states();
            super::apply_connection_capability_limitation(&mut core, None).unwrap();
            assert_eq!(core.registered_capability_states(), before);
            super::apply_connection_capability_limitation(
                &mut core,
                Some("test-no-connection-authority"),
            )
            .unwrap();
            for (id, original) in before {
                let state = core
                    .registered_capability_states()
                    .into_iter()
                    .find(|(key, _)| key == &id)
                    .unwrap()
                    .1;
                assert_eq!(
                    state,
                    if matches!(
                        id.as_str(),
                        "connection:direct" | "background:desktop-maintain-connection"
                    ) {
                        CapabilityState::Unsupported
                    } else {
                        original
                    }
                );
            }
            let descriptors = core.registered_capability_descriptors();
            let direct = descriptors
                .iter()
                .find(|row| row.id() == "connection:direct")
                .unwrap();
            assert_eq!(direct.limitations(), &["test-no-connection-authority"]);
        }
    }
    use ubm_core::central::{Central, CentralConfig};
    use ubm_core::contracts::{
        AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
        BackendInstanceId, Generation,
    };
    use ubm_core::ownership::EffectBatch;

    fn test_core() -> Central {
        let attachment = AttachmentTuple::new(
            AttachmentId::new("test-attachment").expect("id"),
            BackendInstanceId::new("test-backend").expect("id"),
            BackendGeneration::new("test-backend-gen").expect("id"),
            AdapterId::new("test-adapter").expect("id"),
            AdapterGeneration::new("test-adapter-gen").expect("id"),
        );
        let generation = Generation::new("test-gen").expect("generation");
        Central::new(attachment, generation, CentralConfig::default()).expect("core")
    }

    #[test]
    fn every_row_has_a_verdict_and_scenario() {
        assert!(
            !DESKTOP_CAPABILITIES.is_empty(),
            "matrix must not shrink to empty"
        );
        let mut ids = HashSet::new();
        for capability in DESKTOP_CAPABILITIES {
            assert!(!capability.id.is_empty(), "capability id required");
            assert!(!capability.scenario.is_empty(), "scenario required");
            assert!(!capability.note.is_empty(), "report note required");
            assert!(ids.insert(capability.id), "duplicate row {}", capability.id);
            match capability.verdict {
                super::CapabilityVerdict::NarrowOsAdapterNeeded => {
                    assert!(
                        capability.limitation.is_none(),
                        "open work carries no limitation claim"
                    );
                }
                _ => {
                    assert!(
                        capability.limitation.is_some(),
                        "provided/bounded rows name their limitation"
                    );
                }
            }
        }
    }

    /// Machine-checked parity (L7): the code table, the committed
    /// `PARITY_GAPS.md` report, and the frozen backend-contract matrix
    /// agree. A dropped row (L1), a dangling scenario link (L3), or count
    /// drift fails here instead of shipping silently.
    #[test]
    fn parity_report_matches_code_and_frozen_matrix() {
        use std::collections::HashSet;

        use super::CapabilityVerdict;

        let report = include_str!("../PARITY_GAPS.md");
        // Every code row appears in the report with its scenario.
        for capability in DESKTOP_CAPABILITIES {
            assert!(
                report.contains(capability.id),
                "report drops row {}",
                capability.id
            );
            assert!(
                report.contains(capability.scenario),
                "report drops scenario {} for {}",
                capability.scenario,
                capability.id
            );
        }
        // The Counts section matches the code table exactly.
        let mut provides = 0usize;
        let mut narrow = 0usize;
        let mut limitation = 0usize;
        for capability in DESKTOP_CAPABILITIES {
            match capability.verdict {
                CapabilityVerdict::BtleplugProvides => provides += 1,
                CapabilityVerdict::NarrowOsAdapterNeeded => narrow += 1,
                CapabilityVerdict::LimitationCandidate => limitation += 1,
                CapabilityVerdict::OsAdapterProvides => {
                    panic!("OS-adapter verdicts are per-OS overrides, never a base verdict")
                }
            }
        }
        let total = DESKTOP_CAPABILITIES.len();
        for expected in [
            format!("{total} required desktop rows"),
            format!("{provides} btleplug-provides"),
            format!("{narrow} narrow-OS-adapter-needed"),
            format!("{limitation} preapproved-limitation-candidate"),
        ] {
            assert!(
                report.contains(&expected),
                "report counts drifted: missing `{expected}`"
            );
        }
        // The desktop set covers the frozen matrix minus the explicitly
        // scoped-out mobile roles (read-only reference: the frozen file
        // is never modified by this crate).
        let frozen = include_str!("../../../src/backend-contract/capabilities.ts");
        let mut frozen_ids: HashSet<&str> = HashSet::new();
        let mut quoted = false;
        let mut start = 0usize;
        let bytes = frozen.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'\'' {
                if quoted {
                    let token = &frozen[start..index];
                    if is_capability_id(token) {
                        frozen_ids.insert(token);
                    }
                    quoted = false;
                } else {
                    start = index + 1;
                    quoted = true;
                }
            }
            index += 1;
        }
        assert_eq!(
            frozen_ids.len(),
            42,
            "frozen matrix changed size: update this test and the desktop rows together"
        );
        // These rows are not integrated into the desktop provider yet. Native
        // continuation needs a process-owned executor and host-specific wake /
        // restart configuration; absence here is an implementation boundary,
        // not evidence that desktop operating systems cannot offer a wake.
        let scoped_out = [
            "background:apple-restoration",
            "background:android-connected-device-service",
            "background:wake-on-appearance",
            "background:native-resubscribe",
            "background:headless-task",
            "background:wake-notification",
        ];
        for scoped in scoped_out {
            assert!(
                frozen_ids.contains(scoped),
                "frozen matrix lost scoped row {scoped}"
            );
        }
        let desktop_ids: HashSet<&str> = DESKTOP_CAPABILITIES.iter().map(|row| row.id).collect();
        let expected_ids: HashSet<&str> = frozen_ids
            .difference(&scoped_out.into_iter().collect())
            .copied()
            .collect();
        assert_eq!(
            desktop_ids, expected_ids,
            "desktop rows diverged from the frozen matrix"
        );
    }

    /// Frozen capability ids are `namespace:name` (lowercase, dashes).
    fn is_capability_id(token: &str) -> bool {
        let mut parts = token.split(':');
        let head = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        !(parts.next().is_some()
            || head.is_empty()
            || tail.is_empty()
            || !head
                .chars()
                .all(|cell| cell.is_ascii_lowercase() || cell == '-')
            || !tail
                .chars()
                .all(|cell| cell.is_ascii_lowercase() || cell == '-'))
    }

    /// Every per-OS verdict is well-formed and named in the committed
    /// report, so the report and the runtime registration cannot drift.
    #[test]
    fn per_os_verdicts_are_well_formed_and_reported() {
        use super::{CapabilityVerdict, DesktopOs};

        let report = include_str!("../PARITY_GAPS.md");
        let mut missing = Vec::new();
        for capability in DESKTOP_CAPABILITIES {
            let mut seen = HashSet::new();
            for entry in capability.per_os {
                assert!(
                    seen.insert(entry.os),
                    "{} names {} twice",
                    capability.id,
                    entry.os.as_str()
                );
                assert!(!entry.note.is_empty(), "{} needs a note", capability.id);
                match entry.verdict {
                    CapabilityVerdict::NarrowOsAdapterNeeded => assert!(
                        entry.limitation.is_none(),
                        "{} on {}: unsupported carries no limitation claim",
                        capability.id,
                        entry.os.as_str()
                    ),
                    _ => assert!(
                        entry.limitation.is_some(),
                        "{} on {}: provided rows name their limitation",
                        capability.id,
                        entry.os.as_str()
                    ),
                }
                let capability_cell = format!("`{}`", capability.id);
                let named = report.lines().any(|line| {
                    let mut cells = line
                        .split('|')
                        .map(str::trim)
                        .filter(|cell| !cell.is_empty());
                    cells.next() == Some(capability_cell.as_str())
                        && cells.next() == Some(entry.os.as_str())
                });
                if !named {
                    missing.push(format!("| `{}` | {} |", capability.id, entry.os.as_str()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "report is missing per-OS rows: {missing:?}"
        );
        assert_eq!(DesktopOs::ALL.len(), 3);
    }

    /// Windows and Linux have no write-without-response readiness signal.
    /// The row stays a base limitation-candidate in the parity report, and
    /// the runtime answer is `unsupported` with that limitation kept.
    /// macOS stays limited when the vendored probe and callback exist.
    #[test]
    fn windows_and_linux_readiness_is_unsupported_with_its_named_limitation() {
        use super::DesktopOs;
        for os in [DesktopOs::Windows, DesktopOs::Linux] {
            let states = super::desktop_capability_states(Some(os), false);
            let row = states
                .iter()
                .find(|(id, _, _)| *id == "gatt:write-without-response-readiness")
                .expect("readiness row");
            assert_eq!(row.1, ubm_core::central::CapabilityState::Unsupported);
            assert_eq!(row.2, Some("no-readiness-signal"));
        }
        let states = super::desktop_capability_states(Some(DesktopOs::MacOs), false);
        let macos = states
            .iter()
            .find(|(id, _, _)| *id == "gatt:write-without-response-readiness")
            .expect("readiness row");
        #[cfg(btleplug_ubm_write_readiness)]
        {
            assert_eq!(macos.1, ubm_core::central::CapabilityState::Limited);
            assert_eq!(macos.2, Some("deterministic-only"));
        }
        #[cfg(not(btleplug_ubm_write_readiness))]
        {
            assert_eq!(macos.1, ubm_core::central::CapabilityState::Unsupported);
            assert_eq!(macos.2, None);
        }
    }

    /// F11: the macOS readiness row names its patch or its absence. With
    /// the vendored patch the OS adapter provides the legacy watch;
    /// without it no readiness signal exists (the probe answers
    /// `capability.unsupported`), so the row stays open work rather than
    /// claiming btleplug provides it. Each side is asserted under its own
    /// build configuration.
    #[test]
    fn macos_readiness_names_its_patch_or_its_absence() {
        use super::DesktopOs;
        let row = super::DESKTOP_CAPABILITIES
            .iter()
            .find(|row| row.id == "gatt:write-without-response-readiness")
            .expect("readiness row");
        let macos = row
            .per_os
            .iter()
            .find(|entry| entry.os == DesktopOs::MacOs)
            .expect("macOS readiness override");
        #[cfg(btleplug_ubm_write_readiness)]
        {
            assert_eq!(macos.verdict, super::CapabilityVerdict::OsAdapterProvides);
            assert_eq!(macos.limitation, Some("deterministic-only"));
        }
        #[cfg(not(btleplug_ubm_write_readiness))]
        {
            assert_eq!(
                macos.verdict,
                super::CapabilityVerdict::NarrowOsAdapterNeeded
            );
            assert_eq!(macos.limitation, None);
        }
    }

    /// Windows reads `GattSession.MaxPduSize`. Linux reads the
    /// `org.bluez.GattCharacteristic1` MTU. macOS keeps the route and
    /// names the observation unavailable: CoreBluetooth write length is
    /// not an ATT MTU.
    #[test]
    fn effective_mtu_names_its_per_os_observation() {
        use super::DesktopOs;
        let row = super::DESKTOP_CAPABILITIES
            .iter()
            .find(|row| row.id == "connection:effective-mtu")
            .expect("effective-mtu row");
        let windows = row
            .per_os
            .iter()
            .find(|entry| entry.os == DesktopOs::Windows)
            .expect("Windows effective-mtu override");
        assert_eq!(windows.verdict, super::CapabilityVerdict::BtleplugProvides);
        assert_eq!(windows.limitation, Some("winrt-gattsession-max-pdu-size"));
        let linux = row
            .per_os
            .iter()
            .find(|entry| entry.os == DesktopOs::Linux)
            .expect("Linux effective-mtu override");
        assert_eq!(linux.verdict, super::CapabilityVerdict::OsAdapterProvides);
        assert_eq!(linux.limitation, Some("bluez-gatt-characteristic-mtu"));
        let macos = row
            .per_os
            .iter()
            .find(|entry| entry.os == DesktopOs::MacOs)
            .expect("macOS effective-mtu override");
        assert_eq!(macos.verdict, super::CapabilityVerdict::OsAdapterProvides);
        assert_eq!(macos.limitation, Some("corebluetooth-att-mtu-not-observed"));
    }

    #[test]
    fn registration_projects_truth_into_the_core() {
        use ubm_core::central::CapabilityAdmission;
        use ubm_core::contracts::BleErrorCode;

        let mut core = test_core();
        register_desktop_capabilities(&mut core).expect("register");
        // The core's own `parity_rows` covers its six generic rows; the
        // desktop catalog projection is verified row by row through the
        // capability gate against the state this build actually registers.
        // `no-readiness-signal` is unsupported on Windows and Linux while
        // its base parity verdict stays a limitation candidate.
        let os = super::DesktopOs::current();
        let states = super::desktop_capability_states(os, false);
        let mut limited = 0usize;
        let mut unsupported = 0usize;
        for capability in DESKTOP_CAPABILITIES {
            let (_, state, _) = states
                .iter()
                .find(|(id, _, _)| *id == capability.id)
                .expect("registered row");
            match core.check_capability(capability.id, "desktop.probe") {
                Ok(CapabilityAdmission::ProceedWithLimitation) => {
                    assert_eq!(
                        *state,
                        ubm_core::central::CapabilityState::Limited,
                        "{} admitted a row registered unsupported",
                        capability.id
                    );
                    limited += 1;
                }
                Ok(CapabilityAdmission::Proceed) => {
                    panic!("row {} must carry a limitation", capability.id)
                }
                Err(error) => {
                    assert_eq!(
                        *state,
                        ubm_core::central::CapabilityState::Unsupported,
                        "only a row registered unsupported gates closed, got {}",
                        capability.id
                    );
                    assert_eq!(error.code(), BleErrorCode::CapabilityUnsupported);
                    unsupported += 1;
                }
            }
        }
        assert!(limited > 0 && unsupported > 0, "both sides present");
        let _ = EffectBatch::new(64);
    }

    /// Windows and Linux register readiness unsupported and keep the
    /// limitation name on the descriptor. The base verdict stays a
    /// limitation candidate, so the parity counts do not move.
    #[test]
    fn windows_and_linux_register_readiness_unsupported_with_its_name() {
        use ubm_core::central::EvidenceLevel;
        use ubm_core::contracts::BleErrorCode;

        for os in [super::DesktopOs::Windows, super::DesktopOs::Linux] {
            let mut core = test_core();
            super::register_desktop_capabilities_for(&mut core, Some(os), false).expect("register");
            let row = core
                .registered_capability_descriptors()
                .into_iter()
                .find(|row| row.id() == "gatt:write-without-response-readiness")
                .expect("readiness descriptor");
            assert_eq!(row.state(), ubm_core::central::CapabilityState::Unsupported);
            assert_eq!(row.limitations(), &["no-readiness-signal"]);
            assert_eq!(row.evidence_level(), EvidenceLevel::Blocked);
            let refused = core
                .check_capability("gatt:write-without-response-readiness", "desktop.probe")
                .expect_err("readiness is closed");
            assert_eq!(refused.code(), BleErrorCode::CapabilityUnsupported);
        }
    }
}
