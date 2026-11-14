//! Mockable btleplug radio boundary (HOST-DESKTOP).
//!
//! [`RadioBoundary`] is the only seam between [`crate::DesktopCentral`] and
//! the OS radio. Production traffic flows through the btleplug backend
//! (`btleplug_backend.rs`); unit tests drive [`FakeRadio`], a deterministic
//! scriptable boundary that never touches hardware. NO BLE hardware exists
//! on this host, so every radio proof here is a boundary-fault receipt, and
//! the physical central slice stays explicitly queued (see
//! `PARITY_GAPS.md`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Notify;
use ubm_core::contracts::{BleErrorCode, BleErrorDomain};

use crate::errors::DesktopError;

/// Which radio operation a scripted fault targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultOp {
    StartScan,
    StopScan,
    /// Retire callback admission after a physical scan stop.
    ScanIngressRetire,
    /// Fence callbacks admitted before retirement into the central queue.
    ScanIngressFence,
    /// Callback-ingress arm before the physical scan start.
    ScanIngressArm,
    Connect,
    Disconnect,
    Discover,
    #[cfg(test)]
    DiscoveryAccess,
    Read,
    Write,
    Subscribe,
    Unsubscribe,
    /// Hold a synthetic source open after its lease has been retained.
    OrderedOpen,
    #[cfg(test)]
    OrderedSourceFence,
    #[cfg(test)]
    OrderedSourceRetire,
    /// Hold a retained peer-failure page after the desktop central has
    /// authenticated its live route. Used only to prove route retirement
    /// cannot pass that recovery transition.
    PeerFailureRecoveryRead,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    ReclaimAcquire,
    /// Hold a synthetic reclaim release before its source effect.
    #[cfg(any(test, feature = "reclaim-test-support"))]
    ReclaimRelease,
    #[cfg(test)]
    ReclaimForget,
    /// Hold an ordered marker before native admission.
    OrderedMarkerBeforeAdmission,
    /// Hold an ordered marker after native admission.
    OrderedMarkerStart,
    /// Hold a synthetic marker cancellation after its native outcome.
    OrderedMarkerCancel,
    /// Synthetic ordered source barrier.
    NotificationFence,
    /// OS-reported ATT MTU lookup (`mtu()` returns `None`, as withheld).
    Mtu,
    /// Adapter identity readout (`adapter_name()` fails).
    AdapterName,
    /// Connected RSSI read (`read_rssi()` fails or holds).
    Rssi,
    /// Effective ATT MTU read (`read_effective_mtu()` fails or holds).
    EffectiveMtu,
    WriteReadiness,
    ConnectionParameters,
    ConnectionPhy,
    TerminalCleanup,
    RequestPriority,
    /// Adapter power-state read (`adapter_state()` fails or holds).
    AdapterState,
    /// Adapter authorization read (`adapter_authorization()` fails or holds).
    AdapterAuthorization,
    /// Security-state read (`security_state()` fails or holds).
    SecurityState,
    /// Pairing ceremony (`pair()` fails or holds until unblocked or
    /// cancelled through `cancel_pairing()`).
    Pair,
    /// Pairing cancellation (`cancel_pairing()` fails or holds).
    CancelPairing,
    /// Bond removal (`unpair()` fails or holds).
    Unpair,
    /// Address resolution (`resolve_address()` fails).
    ResolveAddress,
    PeerDirectory,
    /// Post-event-loop transport cleanup.
    FinishClose,
}

/// Adapter power state as the OS reports it. `Unknown` is the OS's own
/// answer (not yet determined), never a stand-in for "could not ask".
/// `Resetting` and `Unsupported` are CoreBluetooth's own states
/// (`CBManagerStateResetting` / `CBManagerStateUnsupported`, the legacy
/// addon's `power: 'resetting' | 'unsupported'`); `Unauthorized` is
/// CoreBluetooth refusing this process the adapter
/// (`CBManagerStateUnauthorized`, legacy `authorization: 'denied'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterPowerState {
    PoweredOn,
    PoweredOff,
    Resetting,
    Unsupported,
    Unauthorized,
    Unknown,
}

impl AdapterPowerState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PoweredOn => "powered-on",
            Self::PoweredOff => "powered-off",
            Self::Resetting => "resetting",
            Self::Unsupported => "unsupported",
            Self::Unauthorized => "unauthorized",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this state takes the adapter away from live work: the
    /// adapter-loss teardown runs when a usable adapter reports one of
    /// these (legacy CoreBluetooth `handleAdapterState`, WinRT
    /// `winRtAdapterIsReady`, BlueZ `Powered: false`). `Unknown` is not a
    /// loss: it states no fact.
    #[must_use]
    pub const fn is_loss(self) -> bool {
        matches!(
            self,
            Self::PoweredOff | Self::Resetting | Self::Unsupported | Self::Unauthorized
        )
    }
}

/// Whether the selected adapter is present, as the legacy adapter
/// snapshots reported it (`availability`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterAvailability {
    Available,
    /// The adapter went away (BlueZ adapter object removed or bluetoothd
    /// gone, Windows radio removed) and has not come back.
    Unavailable,
    /// The host has no usable Bluetooth LE central (CoreBluetooth
    /// `Unsupported`).
    Unsupported,
    /// No state was reported yet.
    Unknown,
}

impl AdapterAvailability {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Why the adapter was lost to live work ([`RadioEvent::AdapterLost`] and
/// adapter-state losses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterLossCause {
    PoweredOff,
    Resetting,
    Unsupported,
    Unauthorized,
    /// The adapter object went away (BlueZ `InterfacesRemoved` for
    /// `Adapter1`, Windows radio removal).
    Removed,
    /// The Bluetooth daemon restarted or went away (BlueZ `org.bluez`
    /// name owner changed).
    DaemonRestarted,
    /// Local lifecycle observation overflow; not a physical adapter report.
    SourceObservationOverflow,
}

impl AdapterLossCause {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PoweredOff => "powered-off",
            Self::Resetting => "resetting",
            Self::Unsupported => "unsupported",
            Self::Unauthorized => "unauthorized",
            Self::Removed => "removed",
            Self::DaemonRestarted => "daemon-restarted",
            Self::SourceObservationOverflow => "source-observation-overflow",
        }
    }

    /// The loss an adapter power state reports, if it reports one.
    #[must_use]
    pub const fn from_power(state: AdapterPowerState) -> Option<Self> {
        match state {
            AdapterPowerState::PoweredOff => Some(Self::PoweredOff),
            AdapterPowerState::Resetting => Some(Self::Resetting),
            AdapterPowerState::Unsupported => Some(Self::Unsupported),
            AdapterPowerState::Unauthorized => Some(Self::Unauthorized),
            AdapterPowerState::PoweredOn | AdapterPowerState::Unknown => None,
        }
    }
}

/// Which pre-effect adapter gate an operation passes, per OS: the legacy
/// backend's own admission (PR210 finding 58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionPolicy {
    /// Legacy CoreBluetooth `assertCoreBluetoothOperational`: authorization
    /// first (`permission.denied` / `restricted` / `not-determined`), then
    /// availability, then power.
    CoreBluetooth,
    /// Legacy WinRT `assertWinRtAdapterReady`: availability first, then
    /// authorization (`denied` / `restricted`), then power.
    WinRt,
    /// Legacy BlueZ: no adapter gate (only lifecycle); the OS answers.
    LifecycleOnly,
}

/// Whether the host process runs packaged (MSIX identity) or unpackaged on
/// Windows: the legacy WinRT addon's `deployment` diagnostic
/// (`addon.cpp` `AdapterDeployment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostDeployment {
    Packaged,
    Unpackaged,
}

impl HostDeployment {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Packaged => "packaged",
            Self::Unpackaged => "unpackaged",
        }
    }
}

/// Which D-Bus bus BlueZ is reached on (Linux). `Session` serves a BlueZ
/// service exported on the session bus (mock BlueZ, sandboxed setups): the
/// legacy BlueZ backend's `busKind: 'session'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BluezBus {
    #[default]
    System,
    Session,
}

/// Optional stricter daemon pin supplied by the trusted host.
///
/// Native Linux construction always resolves and binds a unique daemon owner.
/// A supplied pin must match it; it does not attest that any method works.
/// Actual protocol answers remain authoritative, because older BlueZ releases
/// can export an unimplemented LE interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BluezConnectionPolicy {
    /// Restrict construction to this unique D-Bus owner. The radio never
    /// substitutes a later owner or Device1 calls.
    LeBearer { daemon_unique_owner: String },
}

impl BluezConnectionPolicy {
    /// Validate the portable unique-name grammar before allocating a radio.
    /// The Linux boundary also uses libdbus validation and verifies the live
    /// well-known-name owner before any LE operation.
    pub fn validate(&self) -> Result<(), DesktopError> {
        let Self::LeBearer {
            daemon_unique_owner,
        } = self;
        let valid = daemon_unique_owner.len() <= 255
            && daemon_unique_owner.strip_prefix(':').is_some_and(|body| {
                body.contains('.')
                    && body.split('.').all(|part| {
                        !part.is_empty()
                            && part.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                            })
                    })
            });
        if valid {
            Ok(())
        } else {
            Err(DesktopError::new(
                ubm_core::contracts::BleErrorCode::ArgumentInvalid,
                ubm_core::contracts::BleErrorDomain::Core,
                "connection.policy",
            )
            .with_detail("daemonUniqueOwner must be a D-Bus unique name, not a well-known service"))
        }
    }
}

impl BluezBus {
    /// Frozen wire string (legacy `busKind`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Session => "session",
        }
    }
}

/// Whether the D-Bus bus choice can be honoured by this build: the system
/// bus always; the session bus only on Linux with the vendored
/// `bluez-session-bus` patch (`vendor/btleplug/UBM_PATCHES.md` #3), never
/// silently replaced by the system bus.
pub fn bluez_bus_supported(bus: BluezBus) -> Result<(), DesktopError> {
    match bus {
        BluezBus::System => Ok(()),
        BluezBus::Session if cfg!(all(target_os = "linux", btleplug_ubm_bluez_session)) => Ok(()),
        BluezBus::Session => Err(unsupported(
            "adapter.bus",
            if cfg!(target_os = "linux") {
                "this build links crates.io btleplug, which reaches BlueZ on the system bus only"
            } else {
                "a D-Bus bus choice applies to BlueZ on Linux only"
            },
        )),
    }
}

/// Whether the host process may use the adapter, as the OS reports it.
/// `NotDetermined` is the OS's own answer (the user was never asked).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterAuthorization {
    Granted,
    Denied,
    Restricted,
    NotDetermined,
}

impl AdapterAuthorization {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Restricted => "restricted",
            Self::NotDetermined => "not-determined",
        }
    }
}

/// Bond state as the OS reports it. `Unknown` is the OS withholding the
/// fact, never a stand-in for "could not ask" (that is an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BondState {
    Bonded,
    NotBonded,
    Unknown,
}

impl BondState {
    /// Frozen wire string (`PeerSecurityState.bond`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bonded => "bonded",
            Self::NotBonded => "not-bonded",
            Self::Unknown => "unknown",
        }
    }
}

/// Link-security facts the OS reports for one peer. Encryption,
/// authentication and Secure Connections are not measured by any desktop
/// OS adapter here (the legacy backends reported them `unsupported` too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SecurityState {
    pub bond: BondState,
    /// Whether the OS says pairing can be attempted (`None` when the OS
    /// does not say).
    pub pairing_possible: Option<bool>,
}

/// What one OS pairing ceremony achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairOutcome {
    /// This ceremony created the bond.
    Paired(SecurityState),
    /// The peer was already bonded; no ceremony ran.
    AlreadyPaired(SecurityState),
    /// The OS or the peer refused.
    Rejected(Option<String>),
    /// The ceremony was cancelled before a bond existed.
    Cancelled,
}

/// What one unpair achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnpairOutcome {
    Unpaired,
    AlreadyUnpaired,
}

/// LE address type of a peer, as the OS reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressType {
    Public,
    Random,
}

impl AddressType {
    /// Frozen wire string (BlueZ `AddressType` vocabulary).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Random => "random",
        }
    }
}

/// Characteristic facts beyond the five core property bits. `None` means
/// the OS does not report that fact on this platform — never `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CharacteristicAccess {
    pub broadcast: Option<bool>,
    pub authenticated_signed_writes: Option<bool>,
    pub extended_properties: Option<bool>,
    pub reliable_write: Option<bool>,
    pub writable_auxiliaries: Option<bool>,
    pub encrypt_read: Option<bool>,
    pub encrypt_write: Option<bool>,
    pub encrypt_authenticated_read: Option<bool>,
    pub encrypt_authenticated_write: Option<bool>,
    pub secure_read: Option<bool>,
    pub secure_write: Option<bool>,
    pub authorize: Option<bool>,
}

/// The ATT attribute-value ceiling (Core Spec Vol 3 Part F 3.2.9): the
/// longest value any write carries, including a long write.
pub const ATT_MAX_ATTRIBUTE_VALUE: u16 = 512;

/// The LE ATT MTU every link guarantees (Core Spec Vol 3 Part F 3.2.8):
/// the MTU of a link before (or without) an MTU exchange.
pub const ATT_DEFAULT_LE_MTU: u16 = 23;

/// One observed LE connection-parameter snapshot. Interval and supervision
/// timeout are microseconds, the units WinRT converts into before this
/// boundary. `latency` is the peripheral latency in connection events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObservedConnectionParameters {
    pub interval_us: u32,
    pub latency: u16,
    pub supervision_timeout_us: u32,
}

/// A measured direction of the current LE link, never a requested preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlePhy {
    Le1M,
    Le2M,
    LeCoded,
}
impl BlePhy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Le1M => "le-1m",
            Self::Le2M => "le-2m",
            Self::LeCoded => "le-coded",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObservedConnectionPhy {
    pub tx_phy: BlePhy,
    pub rx_phy: BlePhy,
}

/// The largest single write the OS accepts on one link, per write mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WriteLimits {
    pub with_response: u16,
    pub without_response: u16,
}

impl WriteLimits {
    /// Both modes bounded by one ATT payload (`mtu - 3`, never past
    /// [`ATT_MAX_ATTRIBUTE_VALUE`]): the rule for a platform whose OS sends
    /// every write as one ATT PDU and has no per-mode readout. `None` for an
    /// MTU that leaves no payload.
    #[must_use]
    pub fn from_mtu(mtu: u16) -> Option<Self> {
        let payload = mtu
            .checked_sub(3)
            .filter(|payload| *payload > 0)?
            .min(ATT_MAX_ATTRIBUTE_VALUE);
        Some(Self {
            with_response: payload,
            without_response: payload,
        })
    }

    /// A platform whose OS performs the long write itself for a
    /// with-response write larger than one ATT payload (WinRT
    /// `WriteValueAsync`, BlueZ `WriteValue`, the Android stack): a
    /// with-response write carries a whole attribute value
    /// ([`ATT_MAX_ATTRIBUTE_VALUE`]); a write without response stays one
    /// ATT payload of `mtu`. `None` for an MTU that leaves no payload.
    #[must_use]
    pub fn os_long_write(mtu: u16) -> Option<Self> {
        Some(Self {
            with_response: ATT_MAX_ATTRIBUTE_VALUE,
            without_response: Self::from_mtu(mtu)?.without_response,
        })
    }

    /// The limit for one mode.
    #[must_use]
    pub const fn for_mode(self, with_response: bool) -> u16 {
        if with_response {
            self.with_response
        } else {
            self.without_response
        }
    }
}

/// The operation `gatt:maximum-write-length` answers under.
pub(crate) const MAXIMUM_WRITE_LENGTH_OPERATION: &str = "gatt.maximum-write-length";

/// The reported single-write limit for one mode when the admission bound
/// is itself the OS's observation: `limits.for_mode`, or
/// `capability.unavailable` when the OS withholds the limits.
pub(crate) fn observed_write_limit_of(
    limits: Option<WriteLimits>,
    with_response: bool,
) -> Result<u16, DesktopError> {
    limits
        .map(|limits| limits.for_mode(with_response))
        .ok_or_else(|| {
            unobserved_write_limit("the OS has not reported a single-write limit for this link")
        })
}

/// `capability.unavailable` for a `gatt:maximum-write-length` mode the OS
/// has not observed, with the reason.
pub(crate) fn unobserved_write_limit(detail: &str) -> DesktopError {
    DesktopError::new(
        ubm_core::contracts::BleErrorCode::CapabilityUnavailable,
        ubm_core::contracts::BleErrorDomain::Capability,
        MAXIMUM_WRITE_LENGTH_OPERATION,
    )
    .with_detail(detail.to_owned())
}

/// `capability.unsupported` for a radio that cannot answer `operation`.
fn unsupported(operation: &str, detail: &str) -> DesktopError {
    DesktopError::new(
        ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
        ubm_core::contracts::BleErrorDomain::Capability,
        operation,
    )
    .with_detail(detail.to_owned())
}

/// Service filter for scan start, mirroring the validated core request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFilterSpec {
    /// Canonical 128-bit service UUIDs.
    pub service_uuids: Vec<String>,
    /// The caller's duplicate policy (finding 63). `All` asks the OS for
    /// every advertisement (CoreBluetooth `AllowDuplicatesKey: YES`, BlueZ
    /// `DuplicateData: true`); `First` and `Merged` ask it to filter
    /// repeats, as the legacy backends did. WinRT has no OS filter.
    pub duplicates: ubm_core::central::ScanDuplicatePolicy,
    /// The caller's local-name prefix (finding 89), for an OS that can
    /// narrow discovery by name: BlueZ receives it as the
    /// `SetDiscoveryFilter` `Pattern`, as the legacy backend sent it. The
    /// pattern also matches an address prefix, so it only narrows; the host
    /// still filters by name. Radios without a name filter ignore it.
    pub name_prefix: Option<String>,
    pub windows: Option<WindowsScanOptions>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowsScanningMode {
    Passive,
    #[default]
    Active,
    None,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowsScanOptions {
    pub mode: WindowsScanningMode,
    pub allow_extended_advertisements: bool,
}

impl Default for ScanFilterSpec {
    /// No service or name filter, every advertisement.
    fn default() -> Self {
        Self {
            service_uuids: Vec::new(),
            duplicates: ubm_core::central::ScanDuplicatePolicy::All,
            name_prefix: None,
            windows: None,
        }
    }
}

impl ScanFilterSpec {
    /// Whether the OS is asked to report repeated advertisements.
    #[must_use]
    pub fn allow_duplicates(&self) -> bool {
        matches!(self.duplicates, ubm_core::central::ScanDuplicatePolicy::All)
    }
}

/// One manufacturer-data section of an advertisement: the SIG company ID
/// plus the raw payload bytes verbatim. An empty payload is preserved as
/// empty (not dropped): vendor decoders distinguish "section present with
/// no payload" from "section absent" (no entry at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManufacturerData {
    /// SIG company identifier (e.g. `0x006b` Polar, `0x02b2` Oura).
    pub company_id: u16,
    /// Raw section payload bytes, verbatim.
    pub payload: Vec<u8>,
}

/// One service-data section of an advertisement: the service UUID plus the
/// raw payload bytes verbatim. Same empty-payload rule as
/// [`ManufacturerData`]: present-with-empty is preserved, never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceData {
    /// Service UUID (canonical string).
    pub uuid: String,
    /// Raw section payload bytes, verbatim.
    pub payload: Vec<u8>,
}

/// One observed peer: radio identity plus latest advertisement facts.
///
/// Carries the complete discovery fact set the public observation contract
/// needs (`localName`, `serviceUuids`, `manufacturerData`, `serviceData`,
/// `rssi`, `txPowerLevel` per `CompactScanAdvertisement`; the device-kind
/// matcher consumes the first three): null/unknown/empty distinctions are
/// preserved (`local_name: None` vs `Some("")`, empty vs populated
/// vectors), and section payload bytes cross verbatim. One boundary note:
/// btleplug reports services as a plain vector, so "no services observed"
/// is always `[]` (unknown collapses to empty); the matcher treats `[]`
/// like `null` (no match), so no verdict changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSnapshot {
    /// btleplug peripheral id (opaque platform handle string).
    pub id: String,
    /// BLE address string when the OS exposes one (`None` on privacy-masked
    /// CoreBluetooth advertisements).
    pub address: Option<String>,
    /// Advertised service UUIDs (canonical strings).
    pub service_uuids: Vec<String>,
    /// Last RSSI in dBm, when measured.
    pub rssi: Option<i16>,
    /// Complete or shortened local name, when the advertisement carries
    /// one (`None` when absent; `Some("")` is preserved, never coerced).
    pub local_name: Option<String>,
    /// Manufacturer-data sections, payload bytes verbatim. The production
    /// backend sorts by company ID (btleplug reports a map); scripted
    /// boundaries preserve push order.
    pub manufacturer_data: Vec<ManufacturerData>,
    /// Service-data sections, payload bytes verbatim. The production
    /// backend sorts by UUID (btleplug reports a map); scripted boundaries
    /// preserve push order.
    pub service_data: Vec<ServiceData>,
    /// Advertised TX power level in dBm, when the advertisement carries
    /// one (`None` when absent).
    pub tx_power_level: Option<i16>,
    /// Advertisement fields only some platforms report (solicited and
    /// overflow service UUIDs, connectable). Default: none reported.
    pub extras: AdvertisementExtras,
}

/// Advertisement fields only some platforms report (the legacy
/// CoreBluetooth addon did: `native/electron/corebluetooth/index.js`).
/// `None` means the advertisement did not carry the field or the platform
/// does not report it; `Some(vec![])` is a carried field with no entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdvertisementExtras {
    /// Platform capture time in its monotonic epoch, distinct from owner receipt time.
    pub capture_timestamp_ms: Option<u64>,
    /// OS-cached friendly name: device state, never this packet's advertised name.
    pub cached_name: Option<String>,
    /// Address type belonging to this observation's native identity.
    pub address_type: Option<AddressType>,
    /// Solicited service UUIDs (canonical strings).
    pub solicited_service_uuids: Option<Vec<String>>,
    /// Overflow-area service UUIDs (canonical strings; CoreBluetooth
    /// background advertising).
    pub overflow_service_uuids: Option<Vec<String>>,
    /// Whether the advertisement is connectable.
    pub connectable: Option<bool>,
    /// GAP Appearance value (Android `ScanRecord`; no desktop OS reports
    /// it, and the legacy CoreBluetooth, WinRT and BlueZ backends reported
    /// it absent too).
    pub appearance: Option<u16>,
    /// The raw advertisement record bytes (Android `ScanRecord.getBytes`;
    /// no desktop OS reports them, and the legacy desktop backends reported
    /// them absent).
    pub raw_record: Option<Vec<u8>>,
    /// Where this observation's data comes from (finding 122).
    pub source: ObservationSource,
}

/// Where an observation's data comes from (finding 122), so a host labels
/// it honestly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ObservationSource {
    /// This one advertisement's own data (CoreBluetooth, WinRT, Android and
    /// iOS scan callbacks).
    #[default]
    Advertisement,
    /// The OS's current state for the device, merged across advertisements
    /// (BlueZ `Device1` properties, a re-read of a known peripheral): no
    /// single advertisement is reported.
    DeviceState,
}

impl ObservationSource {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advertisement => "advertisement",
            Self::DeviceState => "device-state",
        }
    }
}

/// GATT property flags for one characteristic snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyFlags {
    pub read: bool,
    pub write: bool,
    pub write_without_response: bool,
    pub notify: bool,
    pub indicate: bool,
}

impl PropertyFlags {
    /// Core property bits (`GATT_PROP_*`) for these flags. Radio-neutral:
    /// every boundary implementation maps its OS flags into
    /// [`PropertyFlags`] and the central registers paths from these bits.
    #[must_use]
    pub fn core_bits(self) -> u8 {
        use ubm_core::central::{
            GATT_PROP_INDICATE, GATT_PROP_NOTIFY, GATT_PROP_READ, GATT_PROP_WRITE,
            GATT_PROP_WRITE_NO_RESPONSE,
        };
        let mut bits = 0u8;
        if self.read {
            bits |= GATT_PROP_READ;
        }
        if self.write {
            bits |= GATT_PROP_WRITE;
        }
        if self.write_without_response {
            bits |= GATT_PROP_WRITE_NO_RESPONSE;
        }
        if self.notify {
            bits |= GATT_PROP_NOTIFY;
        }
        if self.indicate {
            bits |= GATT_PROP_INDICATE;
        }
        bits
    }
}

/// CCCD delivery mode a subscriber requires (`require-notification` /
/// `require-indication`). Carried to the radio so a radio that can write
/// the chosen CCCD mode honors it; a radio that cannot select the mode
/// refuses the requirement before any effect instead of accepting a
/// requirement it cannot enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryMode {
    Notification,
    Indication,
}

impl DeliveryMode {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::Indication => "indication",
        }
    }
}

/// CCCD delivery mode the radio actually wrote or observed for one
/// enablement. `Unknown` when the platform neither selects nor reports it
/// (btleplug): reported as observed, never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservedDelivery {
    Notification,
    Indication,
    Unknown,
}

impl ObservedDelivery {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notification => "notification",
            Self::Indication => "indication",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this observation proves `required` was enforced.
    #[must_use]
    pub fn satisfies(self, required: DeliveryMode) -> bool {
        matches!(
            (self, required),
            (Self::Notification, DeliveryMode::Notification)
                | (Self::Indication, DeliveryMode::Indication)
        )
    }
}

/// What the platform says a characteristic read value is. Every backend
/// answers from these two words, with the same meaning:
/// - `ReadResponse`: the platform attributed the value to the ATT read
///   response (Android `onCharacteristicRead`, WinRT `ReadValueAsync`,
///   BlueZ `ReadValue`, CoreBluetooth while the characteristic cannot
///   notify);
/// - `ReadOrNotification`: the platform reports read responses and
///   notifications through one callback (CoreBluetooth
///   `didUpdateValueFor`) and the characteristic could notify when the value
///   arrived, so the value is the read response or a notification/indication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadProvenance {
    ReadResponse,
    ReadOrNotification,
}

impl ReadProvenance {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadResponse => "read-response",
            Self::ReadOrNotification => "read-or-notification",
        }
    }

    /// Parse the frozen wire string; anything else is `None`.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "read-response" => Some(Self::ReadResponse),
            "read-or-notification" => Some(Self::ReadOrNotification),
            _ => None,
        }
    }
}

/// One characteristic read: the value and what the platform says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacteristicRead {
    pub value: Vec<u8>,
    pub provenance: ReadProvenance,
}

impl CharacteristicRead {
    /// A value the platform attributed to the read response.
    #[must_use]
    pub fn read_response(value: Vec<u8>) -> Self {
        Self {
            value,
            provenance: ReadProvenance::ReadResponse,
        }
    }
}

/// One descriptor snapshot (UUID plus occurrence; values travel
/// read/write calls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorSnapshot {
    pub uuid: String,
    /// Occurrence among duplicate descriptor UUIDs under one
    /// characteristic instance (0-based per-UUID count, matching the
    /// central's registration order).
    pub occurrence: u64,
}

/// One characteristic snapshot with occurrence index (duplicates stay
/// addressable through occurrence/path information, never UUID alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacteristicSnapshot {
    pub uuid: String,
    pub occurrence: u64,
    pub properties: PropertyFlags,
    pub descriptors: Vec<DescriptorSnapshot>,
}

/// Why a discovered service has no characteristics.
///
/// `Open` is a normal service. `OsReserved` is a service Windows keeps and
/// was not queried. `AccessDenied` is an ordinary service whose
/// characteristic discovery returned AccessDenied with no ATT byte. Neither
/// is an absent service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceAccess {
    #[default]
    Open,
    OsReserved,
    AccessDenied,
}

/// One service snapshot with occurrence index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedServiceReference {
    pub uuid: String,
    pub occurrence: u64,
}

/// One service snapshot with observed graph metadata; unavailable is not empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub uuid: String,
    pub occurrence: u64,
    pub primary: Option<bool>,
    pub included_services: Option<Vec<IncludedServiceReference>>,
    pub characteristics: Vec<CharacteristicSnapshot>,
    /// `Open` when the radio listed the service normally.
    pub access: ServiceAccess,
}

/// Authoritative identity of one accepted platform GATT graph. This is an
/// internal boundary fence, not a replacement for public core generations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GattSnapshotIdentity {
    pub owner: String,
    pub attachment: u64,
    pub revision: u64,
}

/// Exact source-owner identity for one retained recorder reclaim permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedReclaimGraphKey {
    pub authority_id: u64,
    pub peer_id: String,
    pub connection_generation: String,
    pub identity: GattSnapshotIdentity,
    pub permit_id: u64,
}

/// Exact host request bound to the immutable graph before source receipt issuance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedReclaimPermitRequest {
    pub authority_id: u64,
    pub request_id: u64,
    pub peer_id: String,
    pub connection_generation: String,
    pub identity: GattSnapshotIdentity,
}

/// Result of atomically validating and reserving the native graph.
#[derive(Debug, Clone)]
pub enum OrderedReclaimGraphAcquire {
    Acquired,
    Stale,
    Busy,
    /// Retained authoritative no-effect outcome; exact retries cannot acquire.
    NotAcquired(DesktopError),
}

/// Atomic publication authorization under the retained native graph permit.
#[derive(Debug, Clone)]
pub enum OrderedReclaimGraphCommit {
    Committed,
    Invalidated(DesktopError),
}

/// One central-consumption acknowledgement for an ordered peer source fence.
#[derive(Debug, Clone)]
pub struct PeerSourceFenceAck(Arc<StdMutex<Option<tokio::sync::oneshot::Sender<()>>>>);

/// Exact native lease reservation for a physical-loss fence.
#[cfg(target_os = "linux")]
pub trait PhysicalLossGuard: Send + Sync + std::fmt::Debug {
    /// Idempotently release the pin after the exact barrier is enqueued.
    fn release(&self);
}
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct PhysicalLossPermit {
    pub peer_id: String,
    pub generation: u64,
    pub reason: u8,
    pub token: u64,
    pub(crate) guard: Option<Arc<dyn PhysicalLossGuard>>,
}
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum PhysicalLossReservation {
    Accepted(PhysicalLossPermit),
    Stale,
    Indeterminate(DesktopError),
}

/// Evidence for one physical loss and its ordered source fence.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum PhysicalLossFence {
    /// Exact generation was validated; the fence result may report uncertain tail.
    Accepted(Result<(), DesktopError>),
    /// Stale or an early observation retained by the native owner.
    Stale,
    /// The source could not establish whether the generation was accepted.
    Indeterminate(DesktopError),
}
impl PartialEq for PeerSourceFenceAck {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl PeerSourceFenceAck {
    pub fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (Self(Arc::new(StdMutex::new(Some(sender)))), receiver)
    }
    pub fn acknowledge(&self) {
        if let Some(sender) = self.0.lock().expect("peer source fence").take() {
            let _ = sender.send(());
        }
    }
}

/// Completion owned by a peer-source failure recovery store. The pump keeps
/// its transfer head until central has retained the exact descriptor for the
/// full immutable source-time recipient cut.
#[derive(Debug, Clone)]
pub struct PeerSourceFailureAck(Arc<StdMutex<Option<tokio::sync::oneshot::Sender<()>>>>);
impl PartialEq for PeerSourceFailureAck {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl PeerSourceFailureAck {
    pub fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (Self(Arc::new(StdMutex::new(Some(sender)))), receiver)
    }
    pub fn acknowledge(&self) {
        if let Some(sender) = self.0.lock().expect("peer source failure").take() {
            let _ = sender.send(());
        }
    }
}

/// Radio-side events delivered to the central event loop.
#[derive(Debug, Clone, PartialEq)]
pub enum RadioEvent {
    /// Private BlueZ authority observation. The production radio validates
    /// this physical generation against its exact owned lease before any
    /// public connection/GATT invalidation. Peer-only property hints cannot
    /// substitute for this observed identity.
    #[cfg(target_os = "linux")]
    LinuxPhysicalLost {
        peer_id: String,
        physical_generation: u64,
        reason: u8,
    },
    Advertisement(PeerSnapshot),
    /// A scan callback stamped with its immutable callback-ingress epoch.
    /// The central must never substitute its currently active scan here.
    AdvertisementScoped {
        snapshot: PeerSnapshot,
        ingress_epoch: u64,
    },
    /// The platform event source ended but its scan-ingress cleanup refused.
    ScanIngressSourceClosedFailed(DesktopError),
    Connected(String),
    /// A link transition carrying the core generation admitted by the caller.
    /// The generation is authoritative core identity, not a radio token.
    ConnectionScoped {
        peer_id: String,
        expected_generation: String,
        connected: bool,
        errored: bool,
    },
    Disconnected(String),
    /// The OS reported the link ended with an error (an Android non-zero
    /// GATT status, a CoreBluetooth disconnect `NSError`): a loss, even when
    /// a release was pending. [`RadioEvent::Disconnected`] is a disconnect
    /// the OS reported without saying why.
    Lost(String),
    /// The OS reported an adapter power-state change.
    AdapterState(AdapterPowerState),
    /// The OS reported a change of this process's adapter authorization.
    AdapterAuthorization(AdapterAuthorization),
    /// The adapter went away underneath live work (BlueZ adapter object
    /// removed or `org.bluez` owner change, Windows radio removal). The
    /// central tears down everything live on it.
    AdapterLost(AdapterLossCause),
    /// A lost adapter came back (BlueZ `Adapter1` re-added, Windows radio
    /// re-added). Admission reads power again from later state reports.
    AdapterRestored,
    /// CoreBluetooth reported write-without-response readiness for one
    /// link (`peripheralIsReadyToSendWriteWithoutResponse:`), read after
    /// its queued writes drained.
    WriteReadiness {
        peer_id: String,
        ready: bool,
    },
    /// WinRT reported the link's current connection parameters. Units are
    /// microseconds for the interval and supervision timeout. A scan
    /// response is not a connection-parameter observation.
    ConnectionParameters {
        peer_id: String,
        interval_us: u32,
        latency: u16,
        supervision_timeout_us: u32,
    },
    ConnectionParameterSourceFailed {
        peer_id: String,
        error: DesktopError,
    },
    ConnectionParameterGap {
        peer_id: String,
        missed: u64,
    },
    /// The OS stopped the scan on its own (WinRT watcher `Stopped` without
    /// a stop request): `aborted` when it reported an error, with the OS's
    /// detail.
    ScanTerminated {
        aborted: bool,
        detail: String,
    },
    /// The OS reported a change to one peer's link-security facts (BlueZ
    /// `Device1.Paired`/`Bonded`), read back as the full current state.
    SecurityChanged {
        peer_id: String,
        state: SecurityState,
    },
    /// The OS reports the GATT database changed underneath discovery:
    /// paths invalidate and rediscovery is required (never silently
    /// re-read through stale handles).
    ServicesChanged(String),
    /// Raw platform trigger: the backend must reread authoritative state
    /// before deciding whether an accepted database ended.
    GattInvalidationHint(String),
    /// Authoritative GATT control observation ended or became malformed.
    /// No physical service-change or adapter-loss cause is implied.
    GattWatchFailed(DesktopError),
    /// One accepted peer graph lost authoritative observation. Unlike a
    /// global watch failure, explicit rediscovery may repair this peer.
    GattObservationFailed {
        peer_id: String,
        identity: GattSnapshotIdentity,
        error: DesktopError,
    },
    /// The exact accepted graph's native source overflowed or became
    /// uncertain; this does not identify a physical link or service change.
    GattSourceUncertain {
        peer_id: String,
        identity: GattSnapshotIdentity,
        error: DesktopError,
    },
    /// A native callback could not be associated with any accepted graph.
    /// It is an observable diagnostic, not a graph or connection transition:
    /// callers must not infer ownership from its peer identifier.
    GattSourceUncertainUnassociated {
        peer_id: String,
        error: DesktopError,
    },
    /// A native discovery replaced this exact previously accepted graph.
    /// This does not assert a link failure or an unsolicited service change.
    GattGraphRetired {
        peer_id: String,
        identity: GattSnapshotIdentity,
    },
    /// A previously accepted database ended. Delayed delivery must not
    /// invalidate a newer accepted database of the same peer.
    ServicesChangedScoped {
        peer_id: String,
        identity: GattSnapshotIdentity,
    },
    /// All source records before this fence have been handed to central.
    PeerNotificationSourceFence {
        ack: PeerSourceFenceAck,
    },
    /// Ordered start of a peer notification source generation.
    PeerNotificationSourceStarted {
        peer_id: String,
        generation: u64,
    },
    /// A peer-wide source gap; it cannot be attributed to a characteristic.
    PeerNotificationsLost {
        peer_id: String,
        generation: u64,
        members: Vec<(InstanceKey, u64)>,
        lost: Option<u64>,
    },
    /// Terminal failure of a peer source; no characteristic-specific loss is implied.
    PeerNotificationSourceFailed {
        peer_id: String,
        generation: u64,
        members: Vec<(InstanceKey, u64)>,
        error: DesktopError,
        rejected_loss: Option<u64>,
        tail_uncertain: bool,
    },
    /// A nonterminal source failure retained as bounded recovery evidence.
    PeerNotificationFailureRecoveryOffered {
        descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        members: Vec<(InstanceKey, u64)>,
        teardown: crate::errors::DesktopErrorEvidence,
        ack: PeerSourceFailureAck,
    },
    /// The OS notification broadcast outran one subscription's receiver
    /// (vendored btleplug patch 10): `lost` notifications of the peer were
    /// missed, any of which may have been this instance's. Accounted on the
    /// subscription by its overflow policy.
    NotificationSourceFailed {
        peer_id: String,
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        characteristic_occurrence: u64,
        epoch: u64,
        error: DesktopError,
    },
    NotificationsLost {
        peer_id: String,
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        characteristic_occurrence: u64,
        /// Subscription epoch of the forwarder that missed them (F10).
        epoch: u64,
        lost: u64,
    },
    /// The OS adapter event broadcast outran the radio's receiver
    /// (vendored btleplug patch 10): `skipped` adapter events (connects,
    /// disconnects, state changes, advertisements) were lost.
    EventsLost {
        skipped: u64,
    },
    Notification {
        peer_id: String,
        /// Owning service instance: UUID plus occurrence among duplicate
        /// service UUIDs.
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        /// Occurrence among duplicate characteristic UUIDs under the
        /// service instance. UUID alone never identifies the instance.
        characteristic_occurrence: u64,
        /// Immutable subscription epoch captured when the notifying
        /// forwarder was installed (F10). The central rejects events whose
        /// epoch no longer matches the live routing: a value queued before
        /// a disconnect or service change must never enter a subscription
        /// created after it.
        epoch: u64,
        value: Vec<u8>,
    },
    /// Native callback value with explicit provenance and no invented receipt timestamp.
    NotificationWithProvenance {
        peer_id: String,
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        characteristic_occurrence: u64,
        epoch: u64,
        value: Vec<u8>,
        provenance: ubm_core::central::NotificationProvenance,
        source_generation: u64,
        source_ordinal: u64,
    },
    NotificationWithReceipt {
        peer_id: String,
        service_uuid: String,
        service_occurrence: u64,
        characteristic_uuid: String,
        characteristic_occurrence: u64,
        epoch: u64,
        value: Vec<u8>,
        receipt: ubm_core::central::NotificationReceipt,
    },
}

/// Read-only OS directory fact, independent of locally owned connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryPeer {
    pub peer_id: String,
    pub name: Option<String>,
    /// Directory fact, never evidence that this central acquired a lease.
    pub connection: &'static str,
}

/// Preferred OS connection preset; request acceptance is not an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPriority {
    Balanced,
    HighThroughput,
    LowPower,
}

/// The OS-radio seam. Implementations are `Send + Sync` and shareable: the
/// central holds one `Arc`-capable boundary for the executor lifetime, and
/// every future is `Send` so scan loops and op drivers can move across the
/// shared multi-thread executor. Errors are already contract-attributed
/// [`DesktopError`]s.
///
/// The central bounds every call it makes here with the operation's budget
/// or a liveness backstop, and drops the future when the bound or a cancel
/// wins; implementations must tolerate a dropped call. Link state is read
/// from the connection events ([`RadioEvent::Connected`] /
/// [`RadioEvent::Disconnected`]), never probed after the fact: an
/// operation's result is its own answer.
///
/// Extension rule: new capabilities (pairing, MTU request, connected RSSI,
/// adapter selection) arrive as new methods with default bodies that
/// answer `capability.unsupported`, so existing implementations keep
/// compiling and never claim a capability they do not have.
pub trait RadioBoundary: Send + Sync + 'static {
    /// Register distinct logical memberships and install the collection atomically.
    /// Whether ordered Open requires a native graph identity committed at discovery.
    fn ordered_source_requires_snapshot_identity(&self) -> bool {
        false
    }

    fn prepare_ordered_source(
        &self,
        _request: OrderedSourceRequest,
    ) -> impl Future<Output = Result<OrderedSourceLease, OrderedSourceFailure>> + Send {
        async {
            Err(OrderedSourceFailure {
                error: unsupported("gatt.ordered.source", "ordered source is unavailable"),
                cleanup: None,
            })
        }
    }
    /// Recover an installed source whose prepare waiter ended before delivery.
    fn recover_ordered_source(
        &self,
        _peer_id: &str,
        _collection_id: u64,
    ) -> impl Future<Output = Result<OrderedSourceRecovery, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recover",
                "ordered source is unavailable",
            ))
        }
    }
    /// Start retained-batch delivery after central installed its consumer.
    fn ordered_source_installed(
        &self,
        _lease: &OrderedSourceLease,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.attach",
                "central retained-batch delivery is unavailable",
            ))
        }
    }
    /// Retain one exact logical source claim for a prepared physical activation.
    fn retain_ordered_source_claim(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        _activation: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.claim",
                "ordered source claim is unavailable",
            ))
        }
    }
    /// Confirm this logical claim's cleanup after exact physical or shared release.
    fn confirm_ordered_source_cleanup(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        _activation: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.claim-release",
                "ordered source claim is unavailable",
            ))
        }
    }
    /// Retire one exact logical member only after its source FIFO fence is consumed.
    fn retire_ordered_member(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.member-retire",
                "ordered member retirement is unavailable",
            ))
        }
    }
    /// Register a new logical epoch after the old member fence; source
    /// delivery must remain paused until central rebinds the returned identity.
    fn reregister_ordered_member(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _old: ubm_core::ordered_notifications::Membership,
    ) -> impl Future<Output = Result<ubm_core::ordered_notifications::Membership, DesktopError>> + Send
    {
        async {
            Err(unsupported(
                "gatt.ordered.member-register",
                "ordered member registration is unavailable",
            ))
        }
    }
    /// Retire an interrupted member renewal without losing its exact native
    /// registration ownership. Pending retains all cleanup debt.
    fn abandon_ordered_renewal(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _old: ubm_core::ordered_notifications::Membership,
        _new: Option<ubm_core::ordered_notifications::Membership>,
    ) -> impl Future<Output = Result<OrderedRenewalAbandon, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.renewal-abandon",
                "ordered renewal retirement is unavailable",
            ))
        }
    }
    /// Atomically replace the retired exact member route in the source owner.
    fn rebind_ordered_member(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _old: ubm_core::ordered_notifications::Membership,
        _new: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.rebind",
            "ordered member rebinding is unavailable",
        ))
    }
    fn resume_ordered_member(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.member-resume",
                "ordered member resumption is unavailable",
            ))
        }
    }
    fn poll_ordered_source(
        &self,
        _lease: &OrderedSourceLease,
    ) -> impl Future<Output = Result<ubm_core::ordered_notifications::Poll, DesktopError>> + Send
    {
        async {
            Err(unsupported(
                "gatt.ordered.poll",
                "ordered source is unavailable",
            ))
        }
    }
    fn update_ordered_source(
        &self,
        _lease: &OrderedSourceLease,
        _transition: OrderedSourceTransition,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.update",
            "ordered source is unavailable",
        ))
    }
    /// Attach a coalesced readiness signal to the exact source owner.
    fn attach_ordered_wake(
        &self,
        _lease: &OrderedSourceLease,
        _wake: std::sync::Arc<tokio::sync::Notify>,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.wake",
            "ordered source wake is unavailable",
        ))
    }
    fn ordered_source_status(
        &self,
        _lease: &OrderedSourceLease,
    ) -> Result<OrderedSourceStatus, DesktopError> {
        Err(unsupported(
            "gatt.ordered.status",
            "ordered source is unavailable",
        ))
    }
    /// Increase the byte budget of this exact installed collection.
    fn increase_ordered_byte_capacity(
        &self,
        _lease: &OrderedSourceLease,
        _max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        Err(unsupported(
            "gatt.ordered.increase-capacity",
            "ordered source capacity is unavailable",
        ))
    }
    fn fence_ordered_source(
        &self,
        _lease: &OrderedSourceLease,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.fence",
                "ordered source is unavailable",
            ))
        }
    }
    /// Retire this exact logical source at its native FIFO fence, retaining
    /// its collection and cleanup ownership for draining and release.
    fn retire_ordered_source(
        &self,
        _lease: &OrderedSourceLease,
        _reason: ubm_core::ordered_notifications::EndReason,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.retire",
                "ordered source retirement is unavailable",
            ))
        }
    }
    /// Begin or replay an exact native marker/write transaction. An error
    /// without an authoritative status leaves ownership indeterminate.
    /// True only when start, settlement and idempotent forget are all implemented.
    fn ordered_marker_write_supported(&self) -> bool {
        false
    }
    fn start_ordered_marker_write(
        &self,
        _lease: &OrderedSourceLease,
        _request: &OrderedMarkerWriteRequest,
    ) -> impl Future<Output = Result<OrderedMarkerWriteStatus, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.marker-write",
                "marker/write unavailable",
            ))
        }
    }
    /// Atomically cancel a queued exact start or return its actual admitted status.
    fn cancel_ordered_marker_write(
        &self,
        _lease: &OrderedSourceLease,
        _request: &OrderedMarkerWriteRequest,
    ) -> impl Future<Output = Result<OrderedMarkerWriteStatus, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.marker-write-cancel",
                "marker/write cancellation unavailable",
            ))
        }
    }
    fn ordered_marker_write_status(
        &self,
        _lease: &OrderedSourceLease,
        _request: &OrderedMarkerWriteRequest,
    ) -> impl Future<Output = Result<OrderedMarkerWriteStatus, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.marker-write-status",
                "marker/write unavailable",
            ))
        }
    }
    fn forget_ordered_marker_write(
        &self,
        _lease: &OrderedSourceLease,
        _request: &OrderedMarkerWriteRequest,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.marker-write-forget",
                "marker/write unavailable",
            ))
        }
    }
    /// Reserve/release a downstream marker control slot before native write.
    fn reserve_ordered_marker(
        &self,
        _lease: &OrderedSourceLease,
        _marker_id: u64,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.marker-credit",
            "marker credit unavailable",
        ))
    }
    fn ordered_marker_credit_pending(
        &self,
        _lease: &OrderedSourceLease,
        _marker_id: u64,
    ) -> Result<bool, DesktopError> {
        Err(unsupported(
            "gatt.ordered.marker-credit",
            "marker credit unavailable",
        ))
    }
    fn release_ordered_marker(
        &self,
        _lease: &OrderedSourceLease,
        _marker_id: u64,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.marker-credit",
            "marker credit unavailable",
        ))
    }
    /// Prove the exact source cannot produce further records and all controls
    /// through its seal have been admitted and acknowledged.
    fn seal_ordered_source(
        &self,
        _lease: &OrderedSourceLease,
    ) -> impl Future<Output = Result<OrderedSourceSeal, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.seal",
                "ordered source seal is unavailable",
            ))
        }
    }
    /// Retry backend-owned cleanup until the source route is retired.
    fn release_ordered_source(
        &self,
        _cleanup: &OrderedSourceCleanup,
    ) -> impl Future<Output = Result<OrderedSourceRelease, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.source-release",
                "ordered source is unavailable",
            ))
        }
    }
    /// Capture an exact source membership and native target without enabling notifications.
    fn prepare_notification_activation(
        &self,
        _scope: &InstanceKey,
        _epoch: u64,
        _requested: Option<DeliveryMode>,
        _preferred: Option<DeliveryMode>,
    ) -> impl Future<Output = Result<PreparedNotificationActivation, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.prepare",
                "prepared notification activation is unavailable",
            ))
        }
    }
    /// Dispatch the prepared enable while preserving the backend's late-result ownership.
    fn dispatch_prepared_notification(
        &self,
        _prepared: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<ObservedDelivery, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.enable",
                "prepared notification activation is unavailable",
            ))
        }
    }
    /// Reserve the exact receipt before a dispatch future can be dropped unpolled.
    fn reserve_ordered_notification(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        _prepared: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        Ok(())
    }
    /// Dispatch a group claim with its exact source binding.
    fn dispatch_ordered_notification(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        prepared: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<ObservedDelivery, DesktopError>> + Send {
        self.dispatch_prepared_notification(prepared)
    }
    /// Query an ordered enable before cleanup; Pending retains ownership.
    fn ordered_notification_settlement(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        _prepared: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<OrderedNotificationSettlement, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.settlement",
                "ordered enable settlement is unavailable",
            ))
        }
    }
    /// Release the final physical claim, retaining any failed cleanup debt.
    fn release_prepared_notification(
        &self,
        _prepared: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.release",
                "prepared notification activation is unavailable",
            ))
        }
    }
    /// Release the final physical claim using the same exact group binding.
    fn release_ordered_notification(
        &self,
        _lease: &OrderedSourceLease,
        _member_id: u64,
        _membership: ubm_core::ordered_notifications::Membership,
        prepared: &PreparedNotificationActivation,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        self.release_prepared_notification(prepared)
    }
    /// Reserve the exact native physical lease before resolving a paused source.
    #[cfg(target_os = "linux")]
    fn reserve_physical_loss(
        &self,
        _peer: &str,
        _generation: u64,
        _reason: u8,
    ) -> PhysicalLossReservation {
        PhysicalLossReservation::Indeterminate(unsupported(
            "connection.physical-loss.reserve",
            "physical loss reservation unavailable",
        ))
    }
    /// Revalidate the same reservation atomically while enqueueing the barrier.
    #[cfg(target_os = "linux")]
    fn fence_reserved_physical_loss(
        &self,
        _permit: &PhysicalLossPermit,
    ) -> impl Future<Output = PhysicalLossFence> + Send {
        async {
            PhysicalLossFence::Indeterminate(unsupported(
                "connection.physical-loss.fence",
                "physical loss reservation unavailable",
            ))
        }
    }
    /// Atomically validate a physical loss and enqueue its source barrier.
    #[cfg(target_os = "linux")]
    fn fence_physical_loss(
        &self,
        _peer: &str,
        _generation: u64,
        _reason: u8,
    ) -> impl Future<Output = PhysicalLossFence> + Send {
        async { PhysicalLossFence::Stale }
    }
    /// Private daemon observation admission, independent of cleanup success.
    /// Only the Linux authority radio can validate its owned physical token.
    #[cfg(target_os = "linux")]
    fn accept_physical_loss(
        &self,
        _peer: &str,
        _generation: u64,
        _reason: u8,
    ) -> impl Future<Output = bool> + Send {
        async { false }
    }
    /// Wait until all previously admitted peer notification records reached central.
    fn flush_peer_notification_source(
        &self,
        _peer_id: &str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async { Ok(()) }
    }

    /// Identity of the currently accepted graph, read without radio I/O.
    /// `None` preserves platforms without an authoritative snapshot token.
    fn gatt_snapshot_identity(
        &self,
        _peer_id: &str,
    ) -> Result<Option<GattSnapshotIdentity>, DesktopError> {
        Ok(None)
    }

    /// Whether the complete native recorder-reclaim lifecycle is qualified.
    fn ordered_recording_reclaim_supported(&self) -> bool {
        false
    }

    /// Register one authenticated, process-unique central authority.
    /// `NotDispatched` fences the entire exact request, including every prior
    /// uncertain submission: none has taken effect or can take effect later.
    /// A refusal of one attempt alone must not be reported as this outcome.
    fn open_ordered_reclaim_authority(
        &self,
        _request: u64,
    ) -> impl Future<Output = Result<u64, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph authority is unavailable",
            ))
        }
    }

    /// Retire an authoritative no-effect open-token receipt after fencing all
    /// submissions for that token; exact retries are idempotent until success
    /// is delivered to the caller. Success means
    /// native retirement and its final acknowledgement both completed. A lost
    /// reply retains enough exact identity to resume either phase on retry.
    fn forget_ordered_reclaim_open(
        &self,
        _request: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph open retirement is unavailable",
            ))
        }
    }

    /// Close only after all exact permits have been forgotten; duplicates replay.
    fn close_ordered_reclaim_authority(
        &self,
        _incarnation: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph authority is unavailable",
            ))
        }
    }

    /// Retire an acknowledged authority close receipt without reauthorizing it.
    /// Success includes native retirement acknowledgement; exact retries must
    /// reconcile a lost forget or acknowledgement response until success is
    /// delivered. The caller retires the key immediately after success and
    /// must not submit it again.
    fn forget_ordered_reclaim_authority(
        &self,
        _incarnation: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph authority is unavailable",
            ))
        }
    }

    /// Reserve a source-issued permit receipt before any graph admission.
    /// Exact request retries replay the same source ID.
    /// `NotDispatched` fences the entire exact request, including every prior
    /// uncertain submission; a per-attempt refusal alone is insufficient.
    fn reserve_ordered_reclaim_permit(
        &self,
        _request: &OrderedReclaimPermitRequest,
    ) -> impl Future<Output = Result<u64, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph permit reservation is unavailable",
            ))
        }
    }

    /// Fence and retire an authoritative no-effect permit request. Success
    /// includes native retirement acknowledgement, with exact retries across
    /// lost responses to either phase until success is delivered.
    fn forget_ordered_reclaim_permit_request(
        &self,
        _request: &OrderedReclaimPermitRequest,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph permit request retirement is unavailable",
            ))
        }
    }

    /// Whether reclaim requires a native discovery-commit binding.
    fn ordered_reclaim_requires_snapshot_binding(&self) -> bool {
        false
    }

    /// Bind this exact core connection generation to the accepted native
    /// graph and physical source while discovery admission is held.
    fn bind_ordered_reclaim_snapshot(
        &self,
        _peer_id: &str,
        _connection_generation: &str,
        _identity: &GattSnapshotIdentity,
    ) -> Result<(), DesktopError> {
        Err(unsupported(
            "gatt.ordered.recording-reclaim",
            "native discovery binding is unavailable",
        ))
    }

    /// Retain an exact source-owner permit. Calls with the same key replay
    /// their authoritative result; native invalidation is ordered with acquire.
    /// `NotDispatched` likewise proves no attempt for this exact key has or
    /// can acquire the permit, including attempts with an uncertain response.
    fn acquire_ordered_reclaim_graph(
        &self,
        _key: &OrderedReclaimGraphKey,
    ) -> impl Future<Output = Result<OrderedReclaimGraphAcquire, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph pin is unavailable",
            ))
        }
    }

    /// Freeze a valid permit through release, or report precommit invalidation.
    /// Exact-key repeats replay the same result.
    fn commit_ordered_reclaim_graph(
        &self,
        _key: &OrderedReclaimGraphKey,
    ) -> impl Future<Output = Result<OrderedReclaimGraphCommit, DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph commit is unavailable",
            ))
        }
    }

    /// Release the exact retained permit. An exact duplicate is idempotent.
    fn release_ordered_reclaim_graph(
        &self,
        _key: &OrderedReclaimGraphKey,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph pin is unavailable",
            ))
        }
    }

    /// Forget the exact released permit and acknowledge native retirement.
    /// Return success only after both phases; retain exact identity across
    /// uncertain responses so retries cannot retire a foreign permit. Exact
    /// retries are required until success is delivered; after success the
    /// caller retires the key and must not submit it again.
    fn forget_ordered_reclaim_graph(
        &self,
        _key: &OrderedReclaimGraphKey,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send {
        async {
            Err(unsupported(
                "gatt.ordered.recording-reclaim",
                "native graph receipt retirement is unavailable",
            ))
        }
    }

    /// Discover a graph together with its accepted identity. Token-aware
    /// radios override this to capture both under their admission gate;
    /// the default preserves radios without authoritative snapshot tokens.
    fn discover_scoped(
        &self,
        peer_id: &str,
    ) -> impl Future<
        Output = Result<(Vec<ServiceSnapshot>, Option<GattSnapshotIdentity>), DesktopError>,
    > + Send {
        async {
            let services = self.discover(peer_id).await?;
            Ok((services, self.gatt_snapshot_identity(peer_id)?))
        }
    }

    fn peer_directory_capability_limitations(
        &self,
    ) -> impl Future<Output = Result<(Option<&'static str>, Option<&'static str>), DesktopError>> + Send
    {
        async { Ok((None, None)) }
    }

    fn known_directory_peers(
        &self,
    ) -> impl Future<Output = Result<Vec<DirectoryPeer>, DesktopError>> + Send {
        async {
            Err(unsupported(
                "peers.known",
                "unrestricted OS-known enumeration is unavailable",
            ))
        }
    }

    fn connected_peers(
        &self,
        _services: &[String],
    ) -> impl Future<Output = Result<Vec<DirectoryPeer>, DesktopError>> + Send {
        async {
            Err(unsupported(
                "peers.connected",
                "system-connected retrieval is unavailable",
            ))
        }
    }

    fn bonded_peers(
        &self,
    ) -> impl Future<Output = Result<Vec<DirectoryPeer>, DesktopError>> + Send {
        async {
            Err(unsupported(
                "peers.bonded",
                "the OS does not expose a bonded-peer inventory",
            ))
        }
    }

    fn resolve_peer(
        &self,
        _peer_id: &str,
    ) -> impl Future<Output = Result<Option<DirectoryPeer>, DesktopError>> + Send {
        async {
            Err(unsupported(
                "peers.resolve",
                "identifier retrieval is unavailable",
            ))
        }
    }
    /// Exact identity emitted by this radio's lifecycle and GATT events.
    /// Opaque identities are unchanged; only the platform parser may supply
    /// another spelling. Admission never creates ownership under an alias.
    fn canonical_peer_id(&self, peer_id: &str) -> String {
        peer_id.to_owned()
    }

    fn validate_peer_identity(&self, peer_id: &str, operation: &str) -> Result<(), DesktopError> {
        let canonical = self.canonical_peer_id(peer_id);
        if canonical == peer_id {
            return Ok(());
        }
        Err(DesktopError::new(
            ubm_core::contracts::BleErrorCode::ArgumentInvalid,
            ubm_core::contracts::BleErrorDomain::Connection,
            operation,
        )
        .with_detail(format!(
            "peer identity must use the radio's canonical spelling: {canonical}"
        )))
    }

    fn adapter_name(&self) -> impl Future<Output = Result<String, DesktopError>> + Send + '_;
    fn start_scan(
        &self,
        filter: ScanFilterSpec,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn stop_scan(&self) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn peers(&self) -> impl Future<Output = Result<Vec<PeerSnapshot>, DesktopError>> + Send + '_;
    fn connect<'a>(
        &'a self,
        peer_id: &'a str,
        expected_generation: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// Native deferred mechanism; absence is an explicit refusal, never direct fallback.
    fn connect_when_available<'a>(
        &'a self,
        _peer_id: &'a str,
        _expected_generation: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a {
        async {
            Err(DesktopError::new(
                ubm_core::contracts::BleErrorCode::CapabilityUnsupported,
                ubm_core::contracts::BleErrorDomain::Capability,
                "connection.connect.when-available",
            ))
        }
    }
    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// The disconnect operation's own native observation, when available.
    /// Absence never invents a platform cause from the caller's request.
    fn disconnect_with_observation<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<DisconnectObservation, DesktopError>> + Send + 'a {
        async move {
            self.disconnect(peer_id)
                .await
                .map(|()| DisconnectObservation::default())
        }
    }
    /// Consume only the exact terminal generation whose transition was published.
    /// This is synchronous metadata retirement, never native cleanup.
    fn consume_disconnect_observation(&self, _peer_id: &str, _observation: &DisconnectObservation) {
    }
    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<Vec<ServiceSnapshot>, DesktopError>> + Send + 'a;
    /// Explicit optional acquired transport; no ordinary-operation fallback.
    fn acquire_gatt<'a>(
        &'a self,
        _scope: &'a InstanceKey,
        kind: crate::acquired_gatt::AcquisitionKind,
    ) -> impl Future<Output = Result<crate::acquired_gatt::AcquiredGattTransport, DesktopError>>
    + Send
    + 'a {
        async move {
            Err(unsupported(
                match kind {
                    crate::acquired_gatt::AcquisitionKind::Write => "gatt.acquire-write",
                    crate::acquired_gatt::AcquisitionKind::Notify => "gatt.acquire-notify",
                },
                "this native boundary has no acquired transport",
            ))
        }
    }

    /// Read one characteristic instance. Occurrence selects among
    /// duplicate UUIDs under the service instance (0-based per-UUID
    /// count); UUID alone never identifies the instance.
    fn read_characteristic<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
    ) -> impl Future<Output = Result<CharacteristicRead, DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn write_characteristic<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        with_response: bool,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn read_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        descriptor_uuid: &'a str,
        descriptor_occurrence: u64,
    ) -> impl Future<Output = Result<Vec<u8>, DesktopError>> + Send + 'a;
    #[allow(clippy::too_many_arguments)]
    fn write_descriptor<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        descriptor_uuid: &'a str,
        descriptor_occurrence: u64,
        value: Vec<u8>,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    /// Toggle notifications on one characteristic instance (per-instance
    /// keying: duplicate UUIDs never share a forwarder). On enable,
    /// `epoch` is the central's current subscription epoch for the peer:
    /// the installed forwarder captures it immutably and stamps every
    /// notification it emits, so stale queued values fail the routing check
    /// after a reconnect (F10). Disable ignores it (teardown is keyed by
    /// instance, not generation).
    ///
    /// `requested` carries a hard delivery requirement on enable. A radio
    /// that can write the chosen CCCD mode writes it; a radio that cannot
    /// select the mode must refuse a `Some` requirement before any effect
    /// (`capability.limited`). The returned [`ObservedDelivery`] is what the
    /// radio actually wrote or observed on enable (`Unknown` when the
    /// platform does not report it); disable returns `Unknown`.
    #[allow(clippy::too_many_arguments)]
    fn set_notifications<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
        requested: Option<DeliveryMode>,
    ) -> impl Future<Output = Result<ObservedDelivery, DesktopError>> + Send + 'a;
    /// Enable with an independent soft preference. Radios whose platform
    /// cannot choose a mode keep their ordinary supported selection.
    #[allow(clippy::too_many_arguments)]
    fn set_notifications_with_preference<'a>(
        &'a self,
        peer_id: &'a str,
        service_uuid: &'a str,
        service_occurrence: u64,
        characteristic_uuid: &'a str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
        requested: Option<DeliveryMode>,
        _preferred: Option<DeliveryMode>,
    ) -> impl Future<Output = Result<ObservedDelivery, DesktopError>> + Send + 'a {
        self.set_notifications(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            enable,
            epoch,
            requested,
        )
    }
    /// OS-reported ATT MTU for one peer, or `None` when the OS withholds
    /// it. An unmeasured MTU is never defaulted: the adapter fails writes
    /// closed with `capability.unavailable` instead of guessing 23.
    fn mtu<'a>(&'a self, peer_id: &'a str) -> impl Future<Output = Option<u16>> + Send + 'a;
    /// Next radio event, or `None` when the event source closes (scan
    /// cleanup path). Cancellation is owned by the caller: drop/stop the
    /// future rather than expecting the boundary to abort it.
    fn next_event(&self) -> impl Future<Output = Option<RadioEvent>> + Send + '_;
    fn arm_scan_ingress(
        &self,
        ingress_epoch: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn retire_scan_ingress(
        &self,
        ingress_epoch: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    fn fence_scan_ingress(
        &self,
        ingress_epoch: u64,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_;
    /// Teardown hook: abort live notification forwarders and best-effort
    /// release OS-side CCCDs. Wired into central shutdown so no live OS
    /// subscription outlives the central. Infallible by contract: per-scope
    /// release failures are retained, never raised — the host drains them
    /// via [`RadioBoundary::take_close_failures`] into the shutdown report.
    fn close(&self) -> impl Future<Output = ()> + Send + '_;
    /// Release transport event resources after the central's event consumer
    /// has joined. Refused cleanup remains owned and this hook is retryable.
    /// The completed result accounts for every remaining transport obligation,
    /// including independent cleanup reported by a prior disconnect observation.
    /// A confirmed retry supersedes that obligation's earlier refusal; an owner
    /// that produces `DisconnectObservation::cleanup_failure` must account for
    /// its retained debt here rather than use the default empty implementation.
    fn finish_close(&self) -> impl Future<Output = Vec<DesktopError>> + Send + '_ {
        async { Vec::new() }
    }
    /// Drain close-time release failures retained by the last [`RadioBoundary::close`]
    /// (F14 receipts). Each entry names one characteristic scope whose native
    /// release did not complete; an empty vec means every scope released (or
    /// none was live). Radios without per-scope release accounting keep the
    /// default empty vec.
    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        Vec::new()
    }
    /// RSSI of the live link to `peer_id`, in dBm, as the OS measures it.
    /// A radio that cannot measure it answers `capability.unsupported`.
    fn read_rssi<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<i16, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async { Err(unsupported("peer.rssi", "this radio cannot measure RSSI")) }
    }
    /// Effective ATT MTU of the live link to `peer_id`, when the OS has
    /// actually observed one. Windows reports `GattSession.MaxPduSize`.
    /// Linux reports the `org.bluez.GattCharacteristic1` MTU.
    /// `Ok(None)` means the link is up and no ATT MTU was observed.
    /// CoreBluetooth's `maximumWriteValueLength` is a write capacity, not
    /// that observation. `Ok(None)` is not a link failure and not
    /// `capability.unsupported`.
    fn read_effective_mtu<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<Option<u16>, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async { Ok(None) }
    }
    /// Current adapter power state. A radio that cannot read it answers
    /// `capability.unsupported`.
    fn adapter_state(
        &self,
    ) -> impl Future<Output = Result<AdapterPowerState, DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "adapter.state",
                "this radio cannot read the adapter state",
            ))
        }
    }
    /// Whether this process may use the adapter, as the OS reports it. A
    /// platform without an authorization concept answers
    /// `capability.unsupported`.
    fn adapter_authorization(
        &self,
    ) -> impl Future<Output = Result<AdapterAuthorization, DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "adapter.authorization",
                "this radio has no adapter authorization concept",
            ))
        }
    }
    /// The largest single write the OS accepts on the link to `peer_id`,
    /// per mode, or `None` when the OS withholds it (writes then fail
    /// closed). Defaults to one ATT payload for both modes
    /// ([`WriteLimits::from_mtu`]); a radio whose OS performs long writes
    /// itself answers [`WriteLimits::os_long_write`].
    fn write_limits<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Option<WriteLimits>> + Send + 'a {
        async move { self.mtu(peer_id).await.and_then(WriteLimits::from_mtu) }
    }
    /// The single-write limit the OS observed for one mode, which
    /// `gatt:maximum-write-length` reports. Defaults to the admission limit
    /// ([`Self::write_limits`]); a radio whose admission bound for a mode
    /// is not an observation (BlueZ without a reported MTU, finding 97)
    /// answers `capability.unavailable` for that mode with its reason, and
    /// writes stay admitted up to the bound.
    fn observed_write_limit<'a>(
        &'a self,
        peer_id: &'a str,
        with_response: bool,
    ) -> impl Future<Output = Result<u16, DesktopError>> + Send + 'a {
        async move { observed_write_limit_of(self.write_limits(peer_id).await, with_response) }
    }
    /// Request an OS preferred preset. Acceptance never reports that the
    /// link adopted it; the radio owns the closeable request until replaced
    /// or disconnected.
    fn request_priority<'a>(
        &'a self,
        peer_id: &'a str,
        priority: ConnectionPriority,
    ) -> impl Future<Output = Result<bool, DesktopError>> + Send + 'a {
        let _ = (peer_id, priority);
        async {
            Err(unsupported(
                "connection.request-priority",
                "this radio offers no preferred-parameter control",
            ))
        }
    }

    fn connection_phy<'a>(
        &'a self,
        _peer_id: &'a str,
    ) -> impl Future<Output = Result<ObservedConnectionPhy, DesktopError>> + Send + 'a {
        async {
            Err(unsupported(
                "connection.phy",
                "this radio reports no connection PHY",
            ))
        }
    }
    fn connection_phy_capability_limitation(&self) -> Result<Option<&'static str>, DesktopError> {
        Ok(None)
    }
    fn release_terminal_resources<'a>(
        &'a self,
        _peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a {
        async { Ok(()) }
    }
    fn priority_capability_limitation(&self) -> Result<Option<&'static str>, DesktopError> {
        Ok(None)
    }

    /// Observed LE connection parameters for `peer_id`. Interval and
    /// supervision timeout are microseconds. Changes arrive as
    /// [`RadioEvent::ConnectionParameters`]. Platforms without the API
    /// answer `capability.unsupported`. Windows older than build 22000
    /// answers `capability.unavailable`.
    fn connection_parameters<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<ObservedConnectionParameters, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async {
            Err(unsupported(
                "connection.parameters",
                "this radio reports no connection parameters",
            ))
        }
    }
    /// Whether the link to `peer_id` can take a write without response now
    /// (CoreBluetooth `canSendWriteWithoutResponse`). Readiness changes
    /// arrive as [`RadioEvent::WriteReadiness`].
    fn write_without_response_ready<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<bool, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async {
            Err(unsupported(
                "gatt.write-readiness",
                "this radio reports no write-without-response readiness",
            ))
        }
    }
    /// Whether this radio reports link-security changes itself through
    /// [`RadioEvent::SecurityChanged`]. When it does not, the central
    /// publishes the state each pair/unpair answered instead.
    fn reports_security_changes(&self) -> bool {
        false
    }
    /// Link-security facts for `peer_id`.
    fn security_state<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<SecurityState, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async {
            Err(unsupported(
                "security.state",
                "this radio cannot read link security",
            ))
        }
    }
    /// Run the OS pairing ceremony with `peer_id`. Dropping the future does
    /// not stop an OS ceremony already dispatched: cancellation is
    /// [`RadioBoundary::cancel_pairing`].
    fn pair<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<PairOutcome, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async { Err(unsupported("security.pair", "this radio cannot pair")) }
    }
    /// Ask the OS to stop an in-flight pairing with `peer_id`. `Ok` means
    /// the OS accepted the request (or there was nothing left to cancel);
    /// what the ceremony ended as is the pairing's own answer.
    fn cancel_pairing<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a {
        let _ = peer_id;
        async {
            Err(unsupported(
                "security.cancel-pairing",
                "this radio cannot cancel pairing",
            ))
        }
    }
    /// Remove the OS bond with `peer_id`.
    fn unpair<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<UnpairOutcome, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async { Err(unsupported("security.unpair", "this radio cannot unpair")) }
    }
    /// Resolve an out-of-band LE address to a radio peer id, materializing
    /// the OS device object when the OS has none yet. The peer need not be
    /// advertising for the id to resolve; connecting still needs it.
    fn resolve_address<'a>(
        &'a self,
        address: &'a str,
        address_type: AddressType,
    ) -> impl Future<Output = Result<String, DesktopError>> + Send + 'a {
        let _ = (address, address_type);
        async {
            Err(unsupported(
                "peer.address-targeting",
                "this radio cannot target a peer by address",
            ))
        }
    }
    /// LE address type of `peer_id`, or `None` when the OS does not say.
    fn address_type<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<Option<AddressType>, DesktopError>> + Send + 'a {
        let _ = peer_id;
        async {
            Err(unsupported(
                "peer.address-type",
                "this radio cannot report address types",
            ))
        }
    }
    /// Characteristic facts beyond the core property bits for every
    /// characteristic instance of the last discovery of `peer_id`.
    /// The pre-effect adapter gate this radio's OS applied in the legacy
    /// backend (finding 58). Default: none beyond lifecycle.
    fn admission_policy(&self) -> AdmissionPolicy {
        AdmissionPolicy::LifecycleOnly
    }
    /// An instantiated radio may lack connection authority while still scanning.
    /// Defaults preserve deterministic and other hosts' existing mechanisms.
    fn connection_capability_limitation(&self) -> Option<&'static str> {
        None
    }
    fn when_available_capability_limitation(
        &self,
    ) -> Option<(ubm_core::central::CapabilityState, &'static str)> {
        None
    }
    /// Probe the instantiated connection-parameter API before registering its
    /// capability. A failed platform probe remains an error, never an absence.
    fn connection_parameters_capability_limitation(
        &self,
    ) -> Result<Option<&'static str>, DesktopError> {
        Ok(None)
    }
    /// Whether an adapter loss tears down live work on this radio (finding
    /// 57): the desktop OS radios do, as their legacy backends did. Default:
    /// the loss is reported as a state change only.
    fn tears_down_on_adapter_loss(&self) -> bool {
        false
    }
    /// Whether this radio's OS answers a subscribe to a characteristic that
    /// declares neither notify nor indicate (finding 98): BlueZ's
    /// `StartNotify`, which the legacy BlueZ backend called unchecked. The
    /// central then skips its property gate for such a subscribe. Default:
    /// the gate applies, as the CoreBluetooth, WinRT and mobile legacy
    /// backends had it.
    fn os_answers_unflagged_subscribe(&self) -> bool {
        false
    }
    /// Notification values this radio's bounded ingress refused before the
    /// central read them (finding 131). Each refusal is also reported to
    /// its subscription as upstream loss; this is the radio-wide total.
    /// Default: a radio without a bounded ingress drops none.
    fn ingress_notification_drops(&self) -> u64 {
        0
    }
    /// Read a bounded page from an exact retained peer-source failure.
    /// A host without this capability reports unsupported rather than
    /// fabricating an ordinary notification failure.
    fn read_peer_notification_failure_recovery(
        &self,
        _descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        _offset: u64,
    ) -> impl Future<
        Output = Result<ubm_core::oversized_evidence::PeerSourceFailureChunk, DesktopError>,
    > + Send
    + '_ {
        async {
            Err(unsupported(
                "gatt.notification.source-failure-recovery.read",
                "peer source failure recovery is unavailable",
            ))
        }
    }
    /// Commit an exact fully-read recovery. A backend must validate source
    /// identity and retain the source head if this returns an error.
    fn ack_peer_notification_failure_recovery(
        &self,
        _descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "gatt.notification.source-failure-recovery.ack",
                "peer source failure recovery is unavailable",
            ))
        }
    }
    /// Prepare the private receipt which must be durable before a retained
    /// failure's native FIFO head can advance. This never advances the source
    /// and is replay-idempotent for the exact descriptor.
    fn prepare_peer_notification_failure_recovery_ack(
        &self,
        _descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> impl Future<Output = Result<String, DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "gatt.notification.source-failure-recovery.prepare-ack",
                "peer source failure recovery is unavailable",
            ))
        }
    }
    /// Advance a fully read retained failure's native FIFO head only for the
    /// receipt already made durable by the caller. Repeating that exact
    /// receipt is idempotent and keeps its exact recovery bytes readable for
    /// a durable owner.
    fn begin_peer_notification_failure_recovery_ack(
        &self,
        _descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        _source_ack_id: &str,
    ) -> impl Future<Output = Result<String, DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "gatt.notification.source-failure-recovery.begin-ack",
                "peer source failure recovery is unavailable",
            ))
        }
    }
    /// Retire a source only after its exact begin-ACK receipt has been
    /// durably committed. A stale receipt must leave the retained evidence
    /// available for retry.
    fn retire_peer_notification_failure_recovery_ack(
        &self,
        _descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        _source_ack_id: &str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + '_ {
        async {
            Err(unsupported(
                "gatt.notification.source-failure-recovery.retire-ack",
                "peer source failure recovery is unavailable",
            ))
        }
    }
    fn characteristic_access<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError>> + Send + 'a
    {
        let _ = peer_id;
        async {
            Err(unsupported(
                "gatt.characteristic-access",
                "this radio reports no characteristic facts beyond the core bits",
            ))
        }
    }
}

/// Test ingress bounds (F07): data (notifications) and control
/// (advertisements, connection, service-change) travel separate bounded
/// queues so a data flood can neither exhaust memory nor starve control.
/// 256 data items / 256 KiB bytes holds every existing test workload (max
/// 200 flood events) while proving overload drops under larger floods;
/// 64 control events never fills in tests (control is low-volume).
const FAKE_DATA_CAP: usize = 256;
const FAKE_DATA_BYTES: u64 = 262_144;
const FAKE_CONTROL_CAP: usize = 64;

/// Deterministic scriptable boundary for unit tests. Faults are injected per
/// operation with [`FakeRadio::fail_next`]; queued events are replayed in
/// push order through [`FakeRadio::push_event`]. Like a real OS event stream,
/// [`RadioBoundary::next_event`] pends while the queues are empty and returns
/// `None` only after [`FakeRadio::close_events`] drops the source. Every
/// call is recorded so tests can assert cleanup ordering (e.g. stop-scan
/// after start failure never fires twice).
pub struct FakeRadio {
    acquired: crate::acquired_gatt::synthetic::SyntheticAcquiredGatt,
    pub(crate) ordered: StdMutex<crate::fake_ordered_source::FakeOrderedSource>,
    state: Arc<StdMutex<FakeInner>>,
    notify: Arc<Notify>,
    calls_changed: tokio::sync::watch::Sender<()>,
}

/// One characteristic instance address: (peer, service uuid, service
/// occurrence, characteristic uuid, characteristic occurrence).
pub type InstanceKey = (String, String, u64, String, u64);

/// Exact native activation prepared without writing the CCCD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedNotificationActivation {
    pub scope: InstanceKey,
    pub activation_epoch: u64,
    pub requested: Option<DeliveryMode>,
    pub preferred: Option<DeliveryMode>,
}

/// One centrally owned claim to register at the source before any enable.
#[derive(Debug, Clone)]
pub struct OrderedSourceMember {
    pub member_id: u64,
    pub activation: PreparedNotificationActivation,
    pub delivery: ubm_core::ordered_notifications::DeliveryMode,
}

#[derive(Debug, Clone)]
pub struct OrderedSourceRequest {
    pub peer_id: String,
    pub snapshot_identity: Option<GattSnapshotIdentity>,
    pub connection_generation: String,
    pub database_generation: String,
    pub collection_id: u64,
    pub members: Vec<OrderedSourceMember>,
    pub max_items: usize,
    pub max_bytes: usize,
    pub max_controls: usize,
}

/// A backend-owned source route; close must be retried until acknowledged.
#[derive(Debug, Clone)]
pub struct OrderedSourceLease {
    pub peer_id: String,
    pub connection_generation: String,
    pub collection_id: u64,
    pub source_generation: ubm_core::ordered_notifications::SourceGeneration,
    pub native_group: u64,
    pub members: Vec<(u64, ubm_core::ordered_notifications::Membership)>,
    pub ingress: OrderedSourceIngress,
}

/// Authoritative result of an ordered native enable operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedNotificationSettlement {
    Pending,
    Enabled,
    Released,
}

/// Exact, idempotent marker/write transaction request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMarkerWriteRequest {
    pub marker_id: u64,
    pub connection_generation: String,
    pub snapshot_identity: GattSnapshotIdentity,
    pub target: InstanceKey,
    pub value: Vec<u8>,
    pub with_response: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedMarkerWritePhase {
    /// Marker is admitted; native write initiation has not settled.
    Initiating,
    NotStarted(Option<DesktopError>),
    Started(Option<DesktopError>),
    Settled(Result<(), DesktopError>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedMarkerWriteStatus {
    Pending,
    NotAdmitted(Option<DesktopError>),
    Rejected(DesktopError),
    Admitted {
        ordinal: u64,
        write: OrderedMarkerWritePhase,
    },
}

/// Exact source binding that initiated one shared physical activation.
#[derive(Debug, Clone)]
pub struct OrderedPhysicalOrigin {
    pub lease: OrderedSourceLease,
    pub member_id: u64,
    pub membership: ubm_core::ordered_notifications::Membership,
    pub prepared: PreparedNotificationActivation,
}

/// Exact renewal retirement; uncertainty describes its native callback tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedRenewalAbandon {
    Pending,
    Retired { tail_uncertain: bool },
}

/// Source-finality receipt independent of physical cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedSourceSeal {
    Pending,
    Sealed,
}

/// Whether a source route is released or still owns a pinned receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedSourceRelease {
    Released,
    Pending,
}

/// Which owner admits source records for this specific opened source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedSourceIngress {
    BackendOwned,
    CentralRetainedBatch,
}

#[derive(Debug)]
pub struct OrderedSourceFailure {
    pub error: DesktopError,
    /// Explicit retry key or lease when cleanup is still owned.
    pub cleanup: Option<OrderedSourceCleanup>,
}

#[derive(Debug, Clone)]
pub enum OrderedSourceCleanup {
    Lease(OrderedSourceLease),
    Indeterminate { peer_id: String, collection_id: u64 },
}

#[derive(Debug, Clone)]
pub enum OrderedSourceRecovery {
    /// No source effect was admitted.
    NotStarted,
    /// An exact source open is retained and has not settled.
    Pending,
    Ready(OrderedSourceLease),
}

#[derive(Debug, Clone)]
pub enum OrderedSourceTransition {
    BeginEnable {
        member_id: u64,
    },
    EnableResult {
        member_id: u64,
        outcome: Option<bool>,
    },
    FailEnable {
        member_id: u64,
        outcome: Option<bool>,
        failure: ubm_core::ordered_notifications::Failure,
        tail_uncertain: bool,
    },
    BeginRelease {
        member_id: u64,
    },
    ReleaseResult {
        member_id: u64,
        result: Result<(), String>,
    },
    Close {
        reason: ubm_core::ordered_notifications::EndReason,
        tail_uncertain: bool,
    },
}

#[derive(Debug, Clone)]
pub struct OrderedSourceStatus {
    pub phase: ubm_core::ordered_notifications::Phase,
    pub members: Vec<ubm_core::ordered_notifications::MemberState>,
    pub cleanup_complete: bool,
}

/// One descriptor address: characteristic instance plus descriptor
/// uuid/occurrence.
pub type DescriptorKey = (InstanceKey, String, u64);

/// A release operation's own answer and private native-generation fence.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisconnectObservation {
    /// Optional actual platform observation, not a cause inferred from intent.
    pub platform: Option<crate::errors::PlatformDetail>,
    /// Native physical generation; distinct from public connection/ATT generations.
    pub physical_generation: Option<u64>,
    /// A separately owned cleanup refusal does not negate confirmed link release.
    pub cleanup_failure: Option<crate::errors::DesktopError>,
}

pub(crate) fn bluez_disconnect_observation(reason: u8) -> crate::errors::PlatformDetail {
    crate::errors::PlatformDetail::new("bluez-mgmt", reason.to_string()).with_metadata(
        "disconnectReason",
        crate::errors::PlatformValue::Int(i64::from(reason)),
    )
}

/// One characteristic scope whose close-time native release did not
/// complete (F14 receipt). The scope stays live at the radio: a failed
/// unsubscribe leaves the OS enablement behind, and the shutdown report
/// must say so instead of claiming a clean release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioCloseFailure {
    /// Characteristic instance address (peer, service, occurrences).
    pub scope: InstanceKey,
    /// Radio-side reason (fault detail or OS error string).
    pub detail: String,
}

impl RadioCloseFailure {
    /// Record one unreleased scope with its reason.
    #[must_use]
    pub fn new(scope: InstanceKey, detail: String) -> Self {
        Self { scope, detail }
    }
}

struct FakeInner {
    scan_ingress_epoch: Option<u64>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_permits: HashMap<(u64, u64), (OrderedReclaimGraphKey, bool)>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_permit_receipts: HashMap<(u64, u64), (OrderedReclaimPermitRequest, u64)>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_next_permit: HashMap<u64, u64>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_permit_reserve: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_permit_capacity_refusal: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_acquire: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_commit: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_binding_error: Option<DesktopError>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_authorities: HashMap<u64, (u64, bool)>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_retired_requests: HashSet<u64>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_retired_authorities: HashSet<u64>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_retired_permits: HashMap<u64, (u64, std::collections::BTreeSet<u64>)>,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_next_authority: u64,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_open: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_authority_notdispatched: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_close: bool,
    #[cfg(any(test, feature = "reclaim-test-support"))]
    reclaim_lost_release: bool,
    disconnect_observations: HashMap<String, crate::errors::PlatformDetail>,
    disconnect_cleanup_failures: HashMap<String, DesktopError>,
    disconnect_cleanup_retire_on_finish: std::collections::HashSet<String>,
    #[cfg(target_os = "linux")]
    physical_generations: HashMap<String, u64>,
    #[cfg(target_os = "linux")]
    physical_pins: HashMap<String, u64>,
    #[cfg(target_os = "linux")]
    physical_pending: HashMap<String, u64>,
    #[cfg(target_os = "linux")]
    physical_pin_serial: u64,
    directory_peers: Option<Vec<DirectoryPeer>>,
    bonded_directory_peers: Option<Vec<DirectoryPeer>>,
    known_directory_peers: Option<Vec<DirectoryPeer>>,
    directory_capability_limitations:
        Result<(Option<&'static str>, Option<&'static str>), DesktopError>,
    directory_unblocked_reads: usize,
    canonical_peer_ids: HashMap<String, String>,
    faults: HashMap<FaultOp, VecDeque<(String, Option<crate::errors::PlatformDetail>)>>,
    late_disable_faults: VecDeque<String>,
    /// Scan filters the central passed to `start_scan`, in call order.
    scan_filters: Vec<ScanFilterSpec>,
    /// Peers `peers()` reports (the OS's known-peripheral listing).
    known_peers: Vec<PeerSnapshot>,
    admission: AdmissionPolicy,
    teardown: bool,
    os_answers_unflagged_subscribe: bool,
    /// Separate bounded queues (F07) with a shared push sequence: data
    /// (notifications) bounded by items+bytes with explicit drops, control
    /// (advertisements, connection, service-change) bounded by items. Drain
    /// order follows the shared sequence (push order), so a disconnect never
    /// jumps ahead of earlier data, yet never drops because data is full.
    control: VecDeque<(u64, RadioEvent)>,
    data: VecDeque<(u64, RadioEvent)>,
    seq: u64,
    events_closed: bool,
    /// Bytes currently queued in the data queue (F07 item+byte bound).
    data_bytes: u64,
    /// Data notifications dropped by explicit overload (F07): bounded
    /// ingress never grows memory, and drops are counted, never silent.
    dropped_data: u64,
    /// Control events dropped by explicit overload past [`FAKE_CONTROL_CAP`]:
    /// counted like data drops, never silent. Control is low-volume (tests
    /// push far fewer than 64), so any nonzero count here is a test-design
    /// signal, not expected backpressure.
    dropped_control: u64,
    calls: Vec<String>,
    connected: Vec<String>,
    notifications: Vec<(String, String, bool)>,
    scan_active: bool,
    services: HashMap<String, Vec<ServiceSnapshot>>,
    gatt_identities: HashMap<String, GattSnapshotIdentity>,
    gatt_identity_errors: HashMap<String, DesktopError>,
    scripted_gatt_identities: HashMap<String, VecDeque<Option<GattSnapshotIdentity>>>,
    mtu: HashMap<String, u16>,
    /// Per-instance read payloads: values returned for one addressed
    /// characteristic instance (unset instances return the canned default).
    values: HashMap<InstanceKey, Vec<u8>>,
    /// Provenance the next reads report (unset: `ReadResponse`).
    read_provenance: ReadProvenance,
    /// Live notification registrations: (peer, service, svc occ, char,
    /// char occ) with the CCCD currently enabled. [`FakeRadio::close`]
    /// releases all of them, modelling OS-side unsubscribe at teardown.
    live: HashSet<InstanceKey>,
    live_activations: HashSet<(InstanceKey, u64)>,
    disable_epochs: Vec<(InstanceKey, u64)>,
    /// Close-time release failures retained by the last [`FakeRadio::close`]
    /// (F14 receipts): one entry per scope whose unsubscribe fault fired.
    /// Drained by `take_close_failures`; failed scopes stay in `live`.
    close_failures: Vec<RadioCloseFailure>,
    /// Observed characteristic writes: addressed instance plus the
    /// response mode the adapter selected (`true` = with-response).
    writes: Vec<(InstanceKey, bool)>,
    write_values: Vec<Vec<u8>>,
    /// Observed descriptor reads/writes: addressed descriptor keys.
    descriptor_reads: Vec<DescriptorKey>,
    descriptor_writes: Vec<DescriptorKey>,
    /// Subscription epochs captured at forwarder install, in enable
    /// order: addressed instance plus the epoch the central passed.
    enable_epochs: Vec<(InstanceKey, u64)>,
    /// Delivery requirement carried by each enable, in enable order.
    delivery_requests: Vec<Option<DeliveryMode>>,
    delivery_preferences: Vec<Option<DeliveryMode>>,
    /// Scripted connected RSSI per peer (unset: unmeasured).
    rssi: HashMap<String, i16>,
    /// Scripted effective ATT MTU per peer (unset: the OS withholds it).
    effective_mtu: HashMap<String, u16>,
    /// Scripted adapter power state (unset: unreadable).
    adapter_state: Option<AdapterPowerState>,
    /// Scripted observation overriding the default for the next enables
    /// (default: the requested mode, else `Unknown`).
    scripted_delivery: Option<ObservedDelivery>,
    /// Closed operation gates: an entry means calls to that op wait until
    /// [`FakeRadio::unblock_op`] (contention/failure-injection tests).
    gates: HashMap<FaultOp, Arc<Notify>>,
    /// Scripted adapter authorization (unset: unsupported).
    authorization: Option<AdapterAuthorization>,
    /// Scripted per-peer security state (unset: unsupported).
    security: HashMap<String, SecurityState>,
    /// Scripted outcome of the next ceremony per peer (unset: `Paired`
    /// with a bonded state, like a just-works ceremony).
    pair_outcomes: HashMap<String, PairOutcome>,
    /// Pairings cancelled through `cancel_pairing` while their ceremony
    /// was held: the held ceremony answers `Cancelled`.
    cancelled_pairings: HashSet<String>,
    /// Scripted per-peer write limits (unset: derived from the MTU).
    write_limits: HashMap<String, WriteLimits>,
    /// Scripted (peer, with_response) modes whose write limit is an
    /// admission bound the OS did not observe.
    unobserved_write_limits: HashSet<(String, bool)>,
    /// Scripted address resolutions: (address, type) -> peer id.
    addresses: HashMap<(String, AddressType), String>,
    /// Scripted per-peer address types.
    address_types: HashMap<String, AddressType>,
    /// Scripted per-peer characteristic facts (unset peer: unsupported).
    access: HashMap<String, HashMap<InstanceKey, CharacteristicAccess>>,
    /// Scripted per-peer write-without-response readiness (unset:
    /// unsupported).
    write_readiness: HashMap<String, bool>,
    /// Scripted per-peer connection parameters (unset: unsupported).
    connection_parameters: HashMap<String, ObservedConnectionParameters>,
    connection_phy: HashMap<String, ObservedConnectionPhy>,
    priority_requests: Vec<(String, ConnectionPriority)>,
    connection_parameters_capability: Result<Option<&'static str>, DesktopError>,
    connection_phy_capability: Result<Option<&'static str>, DesktopError>,
    priority_capability: Result<Option<&'static str>, DesktopError>,
    /// One explicitly scripted retained peer-source failure. This exists only
    /// for the central's recovery-boundary regressions; production radios
    /// supply their own native owner and opaque receipt.
    peer_failure_recovery: Option<FakePeerFailureRecovery>,
}

#[derive(Debug, Clone)]
struct FakePeerFailureRecovery {
    descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    bytes: Vec<u8>,
    source_ack_id: String,
    begun: bool,
    retired: bool,
    prepare_failures_remaining: usize,
    retire_failures_remaining: usize,
}

impl Default for FakeRadio {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRadio {
    /// Inject a bounded failure observation into one synthetic source owner.
    pub fn stage_ordered_failure_observation(
        &self,
        peer: &str,
        collection: u64,
        failure: ubm_core::ordered_notifications::Failure,
    ) -> Result<(), DesktopError> {
        use ubm_core::ordered_notifications::Record;
        let mut state = self.ordered.lock().expect("fake ordered source");
        let lease = state
            .leases
            .get(&collection)
            .filter(|lease| lease.peer_id == peer)
            .cloned()
            .ok_or_else(|| {
                DesktopError::subscribe_failed("synthetic ordered source not owned by peer")
            })?;
        state
            .registry
            .with_collection(collection, |owner| {
                owner.admit_failure_record(
                    &lease.source_generation,
                    collection,
                    Record::FailureObservation {
                        failure: Some(failure),
                        source_rejection: None,
                        source_failure_member: None,
                        rejected_loss: None,
                        tail_uncertain: false,
                    },
                )
            })
            .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))
    }
    pub fn stage_ordered_source_loss(
        &self,
        peer: &str,
        count: Option<u64>,
    ) -> Result<(), DesktopError> {
        let _fifo = self.state.lock().expect("fake radio state");
        self.fake_source_loss(peer, count)
    }
    pub fn set_priority_capability_limitation(
        &self,
        limitation: Result<Option<&'static str>, DesktopError>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .priority_capability = limitation;
    }
    pub fn priority_requests(&self) -> Vec<(String, ConnectionPriority)> {
        self.state
            .lock()
            .expect("fake radio state")
            .priority_requests
            .clone()
    }
    #[cfg(test)]
    pub(crate) fn refuse_reclaim_permit_capacity(&self) {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_permit_capacity_refusal = true;
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn fail_reclaim_binding(&self, error: DesktopError) {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_binding_error = Some(error);
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn lose_reclaim_commit_reply(&self) {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_lost_commit = true;
    }

    #[cfg(test)]
    #[expect(
        clippy::type_complexity,
        reason = "the test-only return exposes exact request and receipt keys without creating production-facing fixture types"
    )]
    pub(crate) fn reclaim_test_keys(&self) -> (Vec<(u64, u64)>, Vec<((u64, u64), u64)>) {
        let state = self.state.lock().expect("fake radio state");
        let mut permits: Vec<_> = state.reclaim_permits.keys().copied().collect();
        let mut receipts: Vec<_> = state
            .reclaim_permit_receipts
            .iter()
            .map(|(request, (_, permit))| (*request, *permit))
            .collect();
        permits.sort_unstable();
        receipts.sort_unstable();
        (permits, receipts)
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn lose_reclaim_permit_reserve_reply(&self) {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_lost_permit_reserve = true;
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn refuse_reclaim_authority_before_dispatch(&self) {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_authority_notdispatched = true;
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn lose_reclaim_authority_replies(&self, open: bool, close: bool) {
        let mut state = self.state.lock().expect("fake radio state");
        state.reclaim_lost_open = open;
        state.reclaim_lost_close = close;
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    pub(crate) fn lose_reclaim_replies(&self, acquire: bool, release: bool) {
        let mut state = self.state.lock().expect("fake radio state");
        state.reclaim_lost_acquire = acquire;
        state.reclaim_lost_release = release;
    }

    pub fn set_connection_parameters_capability_limitation(
        &self,
        result: Result<Option<&'static str>, DesktopError>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_parameters_capability = result;
    }
    /// Retain a separately owned cleanup refusal after confirmed link release.
    pub fn set_disconnect_cleanup_failure(&self, peer: &str, error: DesktopError) {
        self.state
            .lock()
            .expect("fake radio state")
            .disconnect_cleanup_failures
            .insert(peer.to_owned(), error);
    }
    /// Confirm retirement of the independently owned cleanup debt.
    pub fn clear_disconnect_cleanup_failure(&self, peer: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .disconnect_cleanup_failures
            .remove(peer);
    }
    /// Script a cleanup retry confirmed during final transport accounting.
    pub fn retire_disconnect_cleanup_at_finish_close(&self, peer: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .disconnect_cleanup_retire_on_finish
            .insert(peer.to_owned());
    }
    /// Script the authoritative detail returned by a synthetic release.
    pub fn set_disconnect_observation(&self, peer: &str, detail: crate::errors::PlatformDetail) {
        self.state
            .lock()
            .expect("fake radio state")
            .disconnect_observations
            .insert(peer.to_owned(), detail);
    }
    /// Set the identity subsequent successful discoveries report.
    #[cfg(test)]
    pub fn force_marker_ordinal(&self, peer_id: &str, ordinal: u64) {
        self.ordered
            .lock()
            .expect("fake ordered source")
            .ordinals
            .insert(peer_id.to_owned(), ordinal);
    }

    /// Resolve a scripted marker/write transaction for synthetic hosts.
    pub fn settle_marker_write(&self, collection_id: u64, marker_id: u64) {
        let mut state = self.ordered.lock().expect("fake ordered source");
        if let Some((_, OrderedMarkerWriteStatus::Admitted { ordinal, write })) = state
            .marker_transactions
            .get_mut(&(collection_id, marker_id))
        {
            let _ = ordinal;
            *write = OrderedMarkerWritePhase::Settled(Ok(()));
        }
    }

    pub fn set_gatt_snapshot_identity(&self, peer_id: &str, identity: GattSnapshotIdentity) {
        let mut state = self.state.lock().expect("fake radio state");
        state.gatt_identity_errors.remove(peer_id);
        state.gatt_identities.insert(peer_id.to_owned(), identity);
    }

    /// Script an actual snapshot read failure without affecting other peers.
    pub fn fail_gatt_snapshot_identity(&self, peer_id: &str, error: DesktopError) {
        let mut state = self.state.lock().expect("fake radio state");
        state.gatt_identities.remove(peer_id);
        state.gatt_identity_errors.insert(peer_id.to_owned(), error);
    }

    /// Script identity reads to reproduce replacement before publication.
    pub fn script_gatt_snapshot_identities(
        &self,
        peer_id: &str,
        identities: Vec<Option<GattSnapshotIdentity>>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .scripted_gatt_identities
            .insert(peer_id.to_owned(), identities.into());
    }

    /// Script the platform parser's canonical spelling without applying
    /// any case-folding policy to other opaque identities.
    pub fn set_canonical_peer_id(&self, alias: &str, canonical: &str) {
        self.state
            .lock()
            .unwrap()
            .canonical_peer_ids
            .insert(alias.to_owned(), canonical.to_owned());
    }

    pub fn new() -> Self {
        Self {
            acquired: crate::acquired_gatt::synthetic::SyntheticAcquiredGatt::default(),
            ordered: StdMutex::new(crate::fake_ordered_source::FakeOrderedSource::default()),
            state: Arc::new(StdMutex::new(FakeInner {
                scan_ingress_epoch: None,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_permits: HashMap::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_permit_receipts: HashMap::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_next_permit: HashMap::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_permit_reserve: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_permit_capacity_refusal: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_acquire: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_commit: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_binding_error: None,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_authorities: HashMap::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_retired_requests: HashSet::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_retired_authorities: HashSet::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_retired_permits: HashMap::new(),
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_next_authority: 1,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_open: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_authority_notdispatched: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_close: false,
                #[cfg(any(test, feature = "reclaim-test-support"))]
                reclaim_lost_release: false,
                disconnect_observations: HashMap::new(),
                disconnect_cleanup_failures: HashMap::new(),
                disconnect_cleanup_retire_on_finish: std::collections::HashSet::new(),
                directory_peers: None,
                bonded_directory_peers: None,
                known_directory_peers: None,
                directory_capability_limitations: Ok((None, None)),
                directory_unblocked_reads: 0,
                canonical_peer_ids: HashMap::new(),
                #[cfg(target_os = "linux")]
                physical_generations: HashMap::new(),
                #[cfg(target_os = "linux")]
                physical_pins: HashMap::new(),
                #[cfg(target_os = "linux")]
                physical_pending: HashMap::new(),
                #[cfg(target_os = "linux")]
                physical_pin_serial: 0,
                faults: HashMap::new(),
                late_disable_faults: VecDeque::new(),
                scan_filters: Vec::new(),
                known_peers: Vec::new(),
                admission: AdmissionPolicy::LifecycleOnly,
                teardown: false,
                os_answers_unflagged_subscribe: false,
                control: VecDeque::new(),
                data: VecDeque::new(),
                seq: 0,
                events_closed: false,
                data_bytes: 0,
                dropped_data: 0,
                dropped_control: 0,
                calls: Vec::new(),
                connected: Vec::new(),
                notifications: Vec::new(),
                scan_active: false,
                services: HashMap::new(),
                gatt_identities: HashMap::new(),
                gatt_identity_errors: HashMap::new(),
                scripted_gatt_identities: HashMap::new(),
                mtu: HashMap::new(),
                values: HashMap::new(),
                read_provenance: ReadProvenance::ReadResponse,
                live: HashSet::new(),
                live_activations: HashSet::new(),
                disable_epochs: Vec::new(),
                close_failures: Vec::new(),
                writes: Vec::new(),
                write_values: Vec::new(),
                descriptor_reads: Vec::new(),
                descriptor_writes: Vec::new(),
                enable_epochs: Vec::new(),
                delivery_requests: Vec::new(),
                delivery_preferences: Vec::new(),
                rssi: HashMap::new(),
                effective_mtu: HashMap::new(),
                adapter_state: None,
                scripted_delivery: None,
                gates: HashMap::new(),
                authorization: None,
                security: HashMap::new(),
                pair_outcomes: HashMap::new(),
                cancelled_pairings: HashSet::new(),
                write_limits: HashMap::new(),
                unobserved_write_limits: HashSet::new(),
                addresses: HashMap::new(),
                address_types: HashMap::new(),
                access: HashMap::new(),
                write_readiness: HashMap::new(),
                connection_parameters: HashMap::new(),
                connection_phy: HashMap::new(),
                priority_requests: Vec::new(),
                priority_capability: Ok(None),
                peer_failure_recovery: None,
                connection_parameters_capability: Ok(None),
                connection_phy_capability: Ok(None),
            })),
            notify: Arc::new(Notify::new()),
            calls_changed: tokio::sync::watch::channel(()).0,
        }
    }

    /// Script the radio's adapter gate (finding 58) and whether an adapter
    /// loss tears down live work (finding 57), as a desktop OS radio does.
    /// Set before opening the central: the central reads both at open.
    pub fn set_os_policy(&self, admission: AdmissionPolicy, teardown: bool) {
        let mut state = self.state.lock().expect("fake radio state");
        state.admission = admission;
        state.teardown = teardown;
    }

    /// Configure one exact retained peer-source failure for deterministic
    /// recovery tests. The supplied bytes must match the frozen descriptor.
    pub fn set_peer_failure_recovery(
        &self,
        descriptor: ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        bytes: Vec<u8>,
    ) {
        assert_eq!(bytes.len() as u64, descriptor.encoded_length);
        self.state
            .lock()
            .expect("fake radio state")
            .peer_failure_recovery = Some(FakePeerFailureRecovery {
            descriptor,
            bytes,
            source_ack_id: "fake-peer-failure-ack-1".to_owned(),
            begun: false,
            retired: false,
            prepare_failures_remaining: 0,
            retire_failures_remaining: 0,
        });
    }

    /// Fail the next retained-source retirement before the fake native owner
    /// advances. A retry with the same durable begin receipt can then prove
    /// the caller did not lose or duplicate the source head.
    pub fn fail_next_peer_failure_recovery_retire(&self) {
        let mut state = self.state.lock().expect("fake radio state");
        let recovery = state
            .peer_failure_recovery
            .as_mut()
            .expect("configure peer failure recovery before injecting retirement failure");
        recovery.retire_failures_remaining += 1;
    }

    /// Fail the next retained-source preparation before a native receipt is
    /// minted. This proves callers retain the already-durable final recipient
    /// ACK and can retry the exact source transition.
    pub fn fail_next_peer_failure_recovery_prepare(&self) {
        let mut state = self.state.lock().expect("fake radio state");
        let recovery = state
            .peer_failure_recovery
            .as_mut()
            .expect("configure peer failure recovery before injecting preparation failure");
        recovery.prepare_failures_remaining += 1;
    }

    pub fn acquired_gatt(&self) -> &crate::acquired_gatt::synthetic::SyntheticAcquiredGatt {
        &self.acquired
    }

    /// Explicit opt-in deterministic directory; never a production fallback.
    pub fn set_directory_peers(&self, peers: Vec<DirectoryPeer>) {
        self.state.lock().expect("fake radio state").directory_peers = Some(peers);
    }

    pub fn set_directory_capability_limitations(
        &self,
        limitations: Result<(Option<&'static str>, Option<&'static str>), DesktopError>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .directory_capability_limitations = limitations;
    }

    pub fn set_known_directory_peers(&self, peers: Vec<DirectoryPeer>) {
        self.state
            .lock()
            .expect("fake radio state")
            .known_directory_peers = Some(peers);
    }

    pub fn set_bonded_directory_peers(&self, peers: Vec<DirectoryPeer>) {
        self.state
            .lock()
            .expect("fake radio state")
            .bonded_directory_peers = Some(peers);
    }

    /// Let exactly the next `count` directory calls bypass the scripted gate.
    pub fn set_directory_unblocked_reads(&self, count: usize) {
        self.state
            .lock()
            .expect("fake radio state")
            .directory_unblocked_reads = count;
    }

    async fn directory_gate(&self) {
        let bypass = {
            let mut state = self.state.lock().expect("fake radio state");
            if state.directory_unblocked_reads == 0 {
                false
            } else {
                state.directory_unblocked_reads -= 1;
                true
            }
        };
        if !bypass {
            self.gate(FaultOp::PeerDirectory).await;
        }
    }

    /// Script whether this radio's OS answers an unflagged subscribe
    /// (finding 98; BlueZ). Read once when a central opens over it.
    pub fn set_os_answers_unflagged_subscribe(&self, answers: bool) {
        self.state
            .lock()
            .expect("fake radio state")
            .os_answers_unflagged_subscribe = answers;
    }

    /// Script the OS's known-peripheral listing `peers()` answers (default:
    /// none). Hosts count it as "peers heard" (Tauri `adapter.state`).
    pub fn set_peers(&self, peers: Vec<PeerSnapshot>) {
        self.state.lock().expect("fake radio state").known_peers = peers;
    }

    /// Scan filters the central passed to `start_scan`, in call order.
    pub fn scan_filters(&self) -> Vec<ScanFilterSpec> {
        self.state
            .lock()
            .expect("fake radio state")
            .scan_filters
            .clone()
    }

    /// Script the OS-reported ATT MTU for `peer_id`. Unset means the OS
    /// withholds it, and writes fail closed as unmeasured.
    pub fn set_mtu(&self, peer_id: &str, mtu: u16) {
        self.state
            .lock()
            .expect("fake radio state")
            .mtu
            .insert(peer_id.to_owned(), mtu);
    }

    /// Script the next failure of `op`; the detail becomes the error detail.
    pub fn fail_next(&self, op: FaultOp, detail: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .faults
            .entry(op)
            .or_default()
            .push_back((detail.to_owned(), None));
    }

    /// Script a disable failure delivered after its operation gate opens.
    pub fn fail_next_disable_after_gate(&self, detail: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .late_disable_faults
            .push_back(detail.to_owned());
    }

    /// [`FakeRadio::fail_next`] whose error also carries the platform's
    /// structured answer (finding 113), as a real OS failure does.
    pub fn fail_next_with_platform(
        &self,
        op: FaultOp,
        detail: &str,
        platform: crate::errors::PlatformDetail,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .faults
            .entry(op)
            .or_default()
            .push_back((detail.to_owned(), Some(platform)));
    }

    /// Queue one radio event for the next [`RadioBoundary::next_event`].
    /// Data (notifications) travel a bounded item+byte queue with explicit
    /// overload drops (counted via [`FakeRadio::dropped_notification_count`]);
    /// control travels a separate bounded queue so a data flood can neither
    /// drop control nor reorder it ahead of earlier data: drain follows push
    /// order (F07). Pushes after [`FakeRadio::close_events`] are dropped,
    /// like OS events after the source closes.
    /// Returns an error if the synthetic ordered source cannot accept the
    /// event; the producer may retry after downstream capacity is available.
    #[must_use = "synthetic source admission can fail under pressure"]
    pub fn push_event(&self, event: RadioEvent) -> Result<(), DesktopError> {
        self.try_push_event(event)
    }

    /// Stage a synthetic event, reporting source pressure to the producer.
    pub fn try_push_event(&self, event: RadioEvent) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        if state.events_closed {
            return Ok(());
        }
        let event = match event {
            RadioEvent::Advertisement(snapshot) => match state.scan_ingress_epoch {
                Some(ingress_epoch) => RadioEvent::AdvertisementScoped {
                    snapshot,
                    ingress_epoch,
                },
                None => RadioEvent::Advertisement(snapshot),
            },
            event => event,
        };
        self.fake_admit_event(&event)?;
        let seq = state.seq.saturating_add(1);
        state.seq = seq;
        match event {
            RadioEvent::Notification { ref value, .. } => {
                let value_len = value.len() as u64;
                if state.data.len() >= FAKE_DATA_CAP
                    || state.data_bytes.saturating_add(value_len) > FAKE_DATA_BYTES
                {
                    state.dropped_data = state.dropped_data.saturating_add(1);
                    return Ok(());
                }
                state.data_bytes = state.data_bytes.saturating_add(value_len);
                state.data.push_back((seq, event));
            }
            control => {
                if state.control.len() >= FAKE_CONTROL_CAP {
                    state.dropped_control = state.dropped_control.saturating_add(1);
                    return Ok(());
                }
                state.control.push_back((seq, control));
            }
        }
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    /// Stage deterministic Linux authority identity; never physical evidence.
    #[cfg(target_os = "linux")]
    pub fn set_physical_generation(&self, peer: &str, generation: u64) {
        let mut state = self.state.lock().expect("fake radio state");
        if state.physical_pins.contains_key(peer) {
            state.physical_pending.insert(peer.to_owned(), generation);
        } else {
            state
                .physical_generations
                .insert(peer.to_owned(), generation);
        }
    }

    /// Close the event source: a pending [`RadioBoundary::next_event`]
    /// resolves to `None` once both queues drain, modelling OS event-source
    /// teardown.
    pub fn close_events(&self) {
        self.state.lock().expect("fake radio state").events_closed = true;
        self.notify.notify_one();
    }

    /// Data notifications dropped by explicit ingress overload (F07).
    pub fn dropped_notification_count(&self) -> u64 {
        self.state.lock().expect("fake radio state").dropped_data
    }

    /// Control events dropped by explicit ingress overload past the control
    /// cap (64). Always zero in well-formed tests; nonzero means the test
    /// pushed more control than the bound admits.
    pub fn dropped_control_count(&self) -> u64 {
        self.state.lock().expect("fake radio state").dropped_control
    }

    /// Bytes currently queued in the data ingress (F07 bound evidence).
    pub fn data_queued_bytes(&self) -> u64 {
        self.state.lock().expect("fake radio state").data_bytes
    }

    /// Recorded call names in order (`"start_scan"`, `"stop_scan"`, ...).
    pub fn calls(&self) -> Vec<String> {
        self.state.lock().expect("fake radio state").calls.clone()
    }

    /// Whether the fake believes a scan is active.
    pub fn scan_active(&self) -> bool {
        self.state.lock().expect("fake radio state").scan_active
    }

    /// Script the discovery snapshot returned for `peer_id`.
    pub fn set_services(&self, peer_id: &str, services: Vec<ServiceSnapshot>) {
        self.state
            .lock()
            .expect("fake radio state")
            .services
            .insert(peer_id.to_owned(), services);
    }

    /// Whether the fake holds an OS link to `peer_id` (set by a completed
    /// `connect`, cleared by a completed `disconnect`).
    pub fn link_connected(&self, peer_id: &str) -> bool {
        self.state
            .lock()
            .expect("fake radio state")
            .connected
            .iter()
            .any(|peer| peer == peer_id)
    }

    /// Script the connected RSSI `read_rssi` reports for `peer_id`.
    pub fn set_rssi(&self, peer_id: &str, rssi: i16) {
        self.state
            .lock()
            .expect("fake radio state")
            .rssi
            .insert(peer_id.to_owned(), rssi);
    }

    /// Script the effective ATT MTU `read_effective_mtu` reports for
    /// `peer_id`. Unset means the OS withholds it.
    pub fn set_effective_mtu(&self, peer_id: &str, mtu: u16) {
        self.state
            .lock()
            .expect("fake radio state")
            .effective_mtu
            .insert(peer_id.to_owned(), mtu);
    }

    /// Script the adapter power state `adapter_state` reports.
    pub fn set_adapter_state(&self, state: AdapterPowerState) {
        self.state.lock().expect("fake radio state").adapter_state = Some(state);
    }

    /// Script the adapter authorization `adapter_authorization` reports.
    pub fn set_adapter_authorization(&self, authorization: AdapterAuthorization) {
        self.state.lock().expect("fake radio state").authorization = Some(authorization);
    }

    /// Script the security state `security_state` reports for `peer_id`.
    /// A completed `pair`/`unpair` updates it like the OS would.
    pub fn set_security(&self, peer_id: &str, state: SecurityState) {
        self.state
            .lock()
            .expect("fake radio state")
            .security
            .insert(peer_id.to_owned(), state);
    }

    /// Script what the next ceremony with `peer_id` ends as.
    pub fn script_pair_outcome(&self, peer_id: &str, outcome: PairOutcome) {
        self.state
            .lock()
            .expect("fake radio state")
            .pair_outcomes
            .insert(peer_id.to_owned(), outcome);
    }

    /// Script per-mode write limits for `peer_id` (unset: one ATT MTU).
    pub fn set_write_limits(&self, peer_id: &str, limits: WriteLimits) {
        self.state
            .lock()
            .expect("fake radio state")
            .write_limits
            .insert(peer_id.to_owned(), limits);
    }

    /// Script one mode's write limit for `peer_id` as an admission bound
    /// the OS did not observe: `observed_write_limit` answers
    /// `capability.unavailable` while writes stay admitted.
    pub fn withhold_observed_write_limit(&self, peer_id: &str, with_response: bool) {
        self.state
            .lock()
            .expect("fake radio state")
            .unobserved_write_limits
            .insert((peer_id.to_owned(), with_response));
    }

    /// Script `resolve_address(address, address_type)` to answer `peer_id`.
    pub fn set_address(&self, address: &str, address_type: AddressType, peer_id: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .addresses
            .insert((address.to_owned(), address_type), peer_id.to_owned());
    }

    /// Script the address type `address_type` reports for `peer_id`.
    pub fn set_address_type(&self, peer_id: &str, address_type: AddressType) {
        self.state
            .lock()
            .expect("fake radio state")
            .address_types
            .insert(peer_id.to_owned(), address_type);
    }

    pub fn set_connection_phy_capability_limitation(
        &self,
        result: Result<Option<&'static str>, DesktopError>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_phy_capability = result;
    }
    pub fn set_connection_phy(&self, peer_id: &str, phy: ObservedConnectionPhy) {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_phy
            .insert(peer_id.to_owned(), phy);
    }
    /// Script the parameters `connection_parameters` reports.
    pub fn set_connection_parameters(
        &self,
        peer_id: &str,
        parameters: ObservedConnectionParameters,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_parameters
            .insert(peer_id.to_owned(), parameters);
    }

    /// Script the readiness `write_without_response_ready` reports.
    pub fn set_write_readiness(&self, peer_id: &str, ready: bool) {
        self.state
            .lock()
            .expect("fake radio state")
            .write_readiness
            .insert(peer_id.to_owned(), ready);
    }

    /// Script the characteristic facts `characteristic_access` reports.
    pub fn set_characteristic_access(
        &self,
        peer_id: &str,
        access: HashMap<InstanceKey, CharacteristicAccess>,
    ) {
        self.state
            .lock()
            .expect("fake radio state")
            .access
            .insert(peer_id.to_owned(), access);
    }

    /// Delivery requirement carried by each enable, in enable order.
    pub fn delivery_requests(&self) -> Vec<Option<DeliveryMode>> {
        self.state
            .lock()
            .expect("fake radio state")
            .delivery_requests
            .clone()
    }

    /// Soft preferences entering the native boundary, independent of requirements.
    pub fn delivery_preferences(&self) -> Vec<Option<DeliveryMode>> {
        self.state
            .lock()
            .expect("fake radio state")
            .delivery_preferences
            .clone()
    }

    /// Script the delivery mode later enables report as observed (models a
    /// radio that wrote a different mode than requested, or none).
    pub fn script_observed_delivery(&self, observed: ObservedDelivery) {
        self.state
            .lock()
            .expect("fake radio state")
            .scripted_delivery = Some(observed);
    }

    /// Script the read payload for one characteristic instance. Reads of
    /// unset instances return the canned default (`0x42`).
    pub fn set_characteristic_value(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
    ) {
        self.state.lock().expect("fake radio state").values.insert(
            (
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ),
            value,
        );
    }

    /// Script the provenance characteristic reads report (models a platform
    /// that fuses read responses and notifications, like CoreBluetooth).
    pub fn script_read_provenance(&self, provenance: ReadProvenance) {
        self.state.lock().expect("fake radio state").read_provenance = provenance;
    }

    /// Number of live notification registrations (CCCDs the fake believes
    /// are OS-enabled). Teardown must drive this to zero.
    pub fn live_subscription_count(&self) -> usize {
        self.state.lock().expect("fake radio state").live.len()
    }

    /// Observed characteristic writes in order: addressed instance key
    /// plus the response mode (`true` = with-response).
    pub fn writes(&self) -> Vec<(InstanceKey, bool)> {
        self.state.lock().expect("fake radio state").writes.clone()
    }

    /// Owned bytes actually accepted by the explicit synthetic radio.
    pub fn write_values(&self) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .expect("fake radio state")
            .write_values
            .clone()
    }

    /// Observed descriptor reads in order: addressed descriptor keys.
    pub fn descriptor_reads(&self) -> Vec<DescriptorKey> {
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_reads
            .clone()
    }

    /// Observed descriptor writes in order: addressed descriptor keys.
    pub fn descriptor_writes(&self) -> Vec<DescriptorKey> {
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_writes
            .clone()
    }

    /// Epochs captured at forwarder install, in enable order: addressed
    /// instance plus the epoch the central passed to `set_notifications`.
    /// Exact active registrations, including distinct activations at one path.
    pub fn live_notification_activations(&self) -> HashSet<(InstanceKey, u64)> {
        self.state
            .lock()
            .expect("fake radio state")
            .live_activations
            .clone()
    }

    /// Successfully released exact registrations.
    pub fn disable_epochs(&self) -> Vec<(InstanceKey, u64)> {
        self.state
            .lock()
            .expect("fake radio state")
            .disable_epochs
            .clone()
    }

    pub fn enable_epochs(&self) -> Vec<(InstanceKey, u64)> {
        self.state
            .lock()
            .expect("fake radio state")
            .enable_epochs
            .clone()
    }

    /// Close the gate on `op`: calls to it wait until
    /// [`FakeRadio::unblock_op`]. Models a stuck OS call for contention
    /// tests (M1/M2/L5/L7).
    pub fn block_op(&self, op: FaultOp) {
        self.state
            .lock()
            .expect("fake radio state")
            .gates
            .entry(op)
            .or_insert_with(|| Arc::new(Notify::new()));
    }

    /// Open the gate on `op`, releasing one waiter (call again per
    /// waiter).
    pub fn unblock_op(&self, op: FaultOp) {
        let notify = self
            .state
            .lock()
            .expect("fake radio state")
            .gates
            .remove(&op);
        if let Some(notify) = notify {
            notify.notify_one();
        }
    }

    /// Open the gate on `op`, releasing every waiter. `Notified` futures
    /// receive `notify_waiters` as soon as they are created; the stored
    /// permit covers a waiter between its gate lookup and that creation.
    pub fn unblock_all(&self, op: FaultOp) {
        let notify = self
            .state
            .lock()
            .expect("fake radio state")
            .gates
            .remove(&op);
        if let Some(notify) = notify {
            notify.notify_waiters();
            notify.notify_one();
        }
    }

    /// Wait while the gate on `op` is closed. The check-then-wait loop is
    /// race-free: `unblock_op` both removes the entry and delivers a
    /// stored permit, so a concurrent unblock either breaks the loop or
    /// releases the wait immediately.
    async fn gate(&self, op: FaultOp) {
        loop {
            let notify = self
                .state
                .lock()
                .expect("fake radio state")
                .gates
                .get(&op)
                .cloned();
            match notify {
                None => break,
                Some(notify) => notify.notified().await,
            }
        }
    }

    fn take_fault(&self, op: FaultOp) -> Option<ScriptedFault> {
        self.state
            .lock()
            .expect("fake radio state")
            .faults
            .get_mut(&op)
            .and_then(VecDeque::pop_front)
            .map(|(detail, platform)| ScriptedFault { detail, platform })
    }

    fn record(&self, call: &str) {
        self.state
            .lock()
            .expect("fake radio state")
            .calls
            .push(call.to_owned());
        self.calls_changed.send_replace(());
    }

    /// Deterministic fixture barrier: resolve when an operation has entered
    /// this boundary the specified number of times. A caller may wrap this
    /// event wait in a watchdog; elapsed time is never a completion signal.
    pub async fn wait_for_calls(&self, call: &str, count: usize) {
        let mut changed = self.calls_changed.subscribe();
        loop {
            changed.borrow_and_update();
            if self
                .calls()
                .iter()
                .filter(|name| name.as_str() == call)
                .count()
                >= count
            {
                return;
            }
            // The sender is owned by self, which remains borrowed throughout.
            changed
                .changed()
                .await
                .expect("fake radio call sender is alive");
        }
    }
}

/// One scripted failure: its detail and, optionally, the platform's
/// structured answer (finding 113).
struct ScriptedFault {
    detail: String,
    platform: Option<crate::errors::PlatformDetail>,
}

/// A scripted failure's error, carrying its platform answer when scripted.
fn scripted(error: DesktopError, platform: Option<crate::errors::PlatformDetail>) -> DesktopError {
    match platform {
        Some(platform) => error.with_platform(platform),
        None => error,
    }
}

/// Build the addressed descriptor key for one descriptor call.
#[allow(clippy::too_many_arguments)]
fn descriptor_key(
    peer_id: &str,
    service_uuid: &str,
    service_occurrence: u64,
    characteristic_uuid: &str,
    characteristic_occurrence: u64,
    descriptor_uuid: &str,
    descriptor_occurrence: u64,
) -> DescriptorKey {
    (
        (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        ),
        descriptor_uuid.to_owned(),
        descriptor_occurrence,
    )
}

#[cfg(target_os = "linux")]
struct FakePhysicalLossGuard {
    state: Arc<StdMutex<FakeInner>>,
    peer: String,
    token: u64,
    active: std::sync::atomic::AtomicBool,
}
#[cfg(target_os = "linux")]
impl std::fmt::Debug for FakePhysicalLossGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakePhysicalLossGuard")
            .field("peer", &self.peer)
            .finish()
    }
}
#[cfg(target_os = "linux")]
impl PhysicalLossGuard for FakePhysicalLossGuard {
    fn release(&self) {
        if !self.active.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let mut s = self.state.lock().expect("fake radio state");
        if s.physical_pins.get(&self.peer) == Some(&self.token) {
            s.physical_pins.remove(&self.peer);
            if let Some(next) = s.physical_pending.remove(&self.peer) {
                s.physical_generations.insert(self.peer.clone(), next);
            }
        }
    }
}
#[cfg(target_os = "linux")]
impl Drop for FakePhysicalLossGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl RadioBoundary for FakeRadio {
    async fn arm_scan_ingress(&self, ingress_epoch: u64) -> Result<(), DesktopError> {
        self.record("arm_scan_ingress");
        self.gate(FaultOp::ScanIngressArm).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::ScanIngressArm) {
            return Err(scripted(DesktopError::scan_start_failed(detail), platform));
        }
        self.state
            .lock()
            .expect("fake radio state")
            .scan_ingress_epoch = Some(ingress_epoch);
        Ok(())
    }

    async fn retire_scan_ingress(&self, ingress_epoch: u64) -> Result<(), DesktopError> {
        self.record("retire_scan_ingress");
        self.gate(FaultOp::ScanIngressRetire).await;
        if let Some(ScriptedFault { detail, platform }) =
            self.take_fault(FaultOp::ScanIngressRetire)
        {
            return Err(scripted(DesktopError::scan_stop_failed(detail), platform));
        }
        let mut state = self.state.lock().expect("fake radio state");
        if state.scan_ingress_epoch == Some(ingress_epoch) {
            state.scan_ingress_epoch = None;
            Ok(())
        } else {
            Err(DesktopError::scan_stop_failed(
                "scan callback ingress epoch is no longer active",
            ))
        }
    }

    async fn fence_scan_ingress(&self, ingress_epoch: u64) -> Result<(), DesktopError> {
        self.record("fence_scan_ingress");
        self.gate(FaultOp::ScanIngressFence).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::ScanIngressFence)
        {
            return Err(scripted(DesktopError::scan_stop_failed(detail), platform));
        }
        let state = self.state.lock().expect("fake radio state");
        if state.scan_ingress_epoch != Some(ingress_epoch) {
            Ok(())
        } else {
            Err(DesktopError::scan_stop_failed(
                "scan callback ingress is still accepting epoch",
            ))
        }
    }

    async fn prepare_notification_activation(
        &self,
        scope: &InstanceKey,
        epoch: u64,
        requested: Option<DeliveryMode>,
        preferred: Option<DeliveryMode>,
    ) -> Result<PreparedNotificationActivation, DesktopError> {
        let handle = PreparedNotificationActivation {
            scope: scope.clone(),
            activation_epoch: epoch,
            requested,
            preferred,
        };
        let mut state = self.ordered.lock().expect("fake ordered source");
        if let Some(old) = state.prepared.get(&(scope.clone(), epoch))
            && old != &handle
        {
            return Err(DesktopError::subscribe_failed("conflicting activation"));
        }
        state
            .prepared
            .insert((scope.clone(), epoch), handle.clone());
        Ok(handle)
    }
    async fn dispatch_prepared_notification(
        &self,
        h: &PreparedNotificationActivation,
    ) -> Result<ObservedDelivery, DesktopError> {
        if self
            .ordered
            .lock()
            .expect("fake ordered source")
            .prepared
            .get(&(h.scope.clone(), h.activation_epoch))
            != Some(h)
        {
            return Err(DesktopError::subscribe_failed("stale activation"));
        }
        struct PendingFakeEnable<'a> {
            radio: &'a FakeRadio,
            key: (InstanceKey, u64),
            completed: bool,
        }
        impl Drop for PendingFakeEnable<'_> {
            fn drop(&mut self) {
                if !self.completed {
                    self.radio
                        .ordered
                        .lock()
                        .expect("fake ordered source")
                        .settlements
                        .insert(self.key.clone(), OrderedNotificationSettlement::Released);
                }
            }
        }
        let key = (h.scope.clone(), h.activation_epoch);
        self.ordered
            .lock()
            .expect("fake ordered source")
            .settlements
            .insert(key.clone(), OrderedNotificationSettlement::Pending);
        let mut guard = PendingFakeEnable {
            radio: self,
            key: key.clone(),
            completed: false,
        };
        let result = self
            .set_notifications_with_preference(
                &h.scope.0,
                &h.scope.1,
                h.scope.2,
                &h.scope.3,
                h.scope.4,
                true,
                h.activation_epoch,
                h.requested,
                h.preferred,
            )
            .await;
        self.ordered
            .lock()
            .expect("fake ordered source")
            .settlements
            .insert(
                key,
                if result.is_ok() {
                    OrderedNotificationSettlement::Enabled
                } else {
                    OrderedNotificationSettlement::Released
                },
            );
        guard.completed = true;
        result
    }
    fn reserve_ordered_notification(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        h: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        self.fake_claim(l, id, m, h)?;
        let mut s = self.ordered.lock().expect("fake ordered source");
        let key = (h.scope.clone(), h.activation_epoch);
        let origin = (l.collection_id, id, m);
        if s.origins
            .get(&key)
            .is_some_and(|existing| *existing != origin)
        {
            return Err(DesktopError::subscribe_failed(
                "physical activation already has an origin",
            ));
        }
        s.origins.insert(key.clone(), origin);
        s.settlements
            .entry(key)
            .or_insert(OrderedNotificationSettlement::Released);
        drop(s);
        self.record("ordered_reserve");
        Ok(())
    }
    async fn dispatch_ordered_notification(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        h: &PreparedNotificationActivation,
    ) -> Result<ObservedDelivery, DesktopError> {
        self.fake_claim(l, id, m, h)?;
        self.dispatch_prepared_notification(h).await
    }
    async fn ordered_notification_settlement(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        h: &PreparedNotificationActivation,
    ) -> Result<OrderedNotificationSettlement, DesktopError> {
        if self.fake_claim(l, id, m, h).is_err() {
            let s = self.ordered.lock().expect("fake ordered source");
            if s.origins.get(&(h.scope.clone(), h.activation_epoch))
                != Some(&(l.collection_id, id, m))
            {
                return Err(DesktopError::subscribe_failed("stale physical origin"));
            }
        }
        let s = self.ordered.lock().expect("fake ordered source");
        Ok(s.settlements
            .get(&(h.scope.clone(), h.activation_epoch))
            .copied()
            .unwrap_or(OrderedNotificationSettlement::Released))
    }

    async fn release_ordered_notification(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        h: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        self.ordered_notification_settlement(l, id, m, h).await?;
        self.release_prepared_notification(h).await
    }
    async fn release_prepared_notification(
        &self,
        h: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        match self
            .ordered
            .lock()
            .expect("fake ordered source")
            .prepared
            .get(&(h.scope.clone(), h.activation_epoch))
        {
            None => return Ok(()),
            Some(current) if current != h => {
                return Err(DesktopError::subscribe_failed("stale activation"));
            }
            Some(_) => {}
        }
        self.set_notifications_with_preference(
            &h.scope.0,
            &h.scope.1,
            h.scope.2,
            &h.scope.3,
            h.scope.4,
            false,
            h.activation_epoch,
            h.requested,
            h.preferred,
        )
        .await?;
        let mut ordered = self.ordered.lock().expect("fake ordered source");
        ordered
            .prepared
            .remove(&(h.scope.clone(), h.activation_epoch));
        ordered.settlements.insert(
            (h.scope.clone(), h.activation_epoch),
            OrderedNotificationSettlement::Released,
        );
        Ok(())
    }
    async fn prepare_ordered_source(
        &self,
        request: OrderedSourceRequest,
    ) -> Result<OrderedSourceLease, OrderedSourceFailure> {
        let lease = self.fake_prepare_ordered(request)?;
        self.record("ordered_open");
        self.gate(FaultOp::OrderedOpen).await;
        Ok(lease)
    }
    async fn recover_ordered_source(
        &self,
        peer: &str,
        id: u64,
    ) -> Result<OrderedSourceRecovery, DesktopError> {
        let state = self.ordered.lock().expect("fake ordered source");
        Ok(match state.leases.get(&id) {
            Some(l) if l.peer_id == peer => OrderedSourceRecovery::Ready(l.clone()),
            Some(_) => return Err(DesktopError::subscribe_failed("stale peer")),
            None => OrderedSourceRecovery::NotStarted,
        })
    }
    async fn retain_ordered_source_claim(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        a: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        self.fake_claim(l, id, m, a)
    }
    async fn confirm_ordered_source_cleanup(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        m: ubm_core::ordered_notifications::Membership,
        a: &PreparedNotificationActivation,
    ) -> Result<(), DesktopError> {
        self.fake_claim(l, id, m, a)
    }
    fn update_ordered_source(
        &self,
        l: &OrderedSourceLease,
        t: OrderedSourceTransition,
    ) -> Result<(), DesktopError> {
        self.fake_update(l, t)
    }
    fn attach_ordered_wake(
        &self,
        lease: &OrderedSourceLease,
        wake: std::sync::Arc<tokio::sync::Notify>,
    ) -> Result<(), DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        let source = crate::fake_ordered_source::FakeOrderedSource::source(lease);
        state
            .registry
            .set_wake(lease.collection_id, &source, wake)
            .map_err(|e| DesktopError::subscribe_failed(format!("ordered wake: {e:?}")))
    }
    fn ordered_source_status(
        &self,
        l: &OrderedSourceLease,
    ) -> Result<OrderedSourceStatus, DesktopError> {
        self.fake_status(l)
    }
    fn increase_ordered_byte_capacity(
        &self,
        lease: &OrderedSourceLease,
        max_bytes: usize,
    ) -> Result<usize, DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        let source = crate::fake_ordered_source::FakeOrderedSource::source(lease);
        if !state.registry.owns_source(lease.collection_id, &source) {
            return Err(DesktopError::new(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "gatt.ordered.increase-capacity",
            ));
        }
        let previous = state
            .registry
            .with_collection(lease.collection_id, |owner| {
                owner.increase_byte_capacity(&source.generation, lease.collection_id, max_bytes)
            })
            .map_err(|error| {
                DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.ordered.increase-capacity",
                )
                .with_detail(format!("{error:?}"))
            })?;
        state.flush_pending_records(&source)?;
        Ok(previous)
    }
    async fn retire_ordered_member(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        old: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        let mut s = self.ordered.lock().expect("fake ordered source");
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        if !s.registry.owns_source(l.collection_id, &source)
            || !s
                .claims
                .get(&l.collection_id)
                .is_some_and(|v| v.iter().any(|(i, m, _)| *i == id && *m == old))
        {
            return Err(DesktopError::subscribe_failed("stale member"));
        }
        if let Some(r) = s.renewals.get(&(l.collection_id, id)) {
            if r.old == old {
                return Ok(());
            }
            if !r.resumed {
                return Err(DesktopError::subscribe_failed("member renewal pending"));
            }
        }
        s.registry.retire_membership(&source, old);
        s.buffered.entry(l.collection_id).or_default();
        s.renewals.insert(
            (l.collection_id, id),
            crate::fake_ordered_source::FakeRenewal {
                old,
                new: None,
                rebound: false,
                resumed: false,
            },
        );
        Ok(())
    }
    async fn reregister_ordered_member(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        old: ubm_core::ordered_notifications::Membership,
    ) -> Result<ubm_core::ordered_notifications::Membership, DesktopError> {
        let mut s = self.ordered.lock().expect("fake ordered source");
        let r = s
            .renewals
            .get(&(l.collection_id, id))
            .filter(|r| r.old == old && !r.resumed)
            .ok_or_else(|| DesktopError::subscribe_failed("stale renewal"))?;
        if let Some(new) = r.new {
            return Ok(new);
        }
        let new = ubm_core::ordered_notifications::Membership {
            member: s
                .next_id()
                .ok_or_else(|| DesktopError::subscribe_failed("source identity exhausted"))?,
            epoch: 1,
        };
        s.renewals
            .get_mut(&(l.collection_id, id))
            .expect("retained renewal")
            .new = Some(new);
        Ok(new)
    }
    async fn abandon_ordered_renewal(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        old: ubm_core::ordered_notifications::Membership,
        new: Option<ubm_core::ordered_notifications::Membership>,
    ) -> Result<OrderedRenewalAbandon, DesktopError> {
        let state = self
            .ordered
            .lock()
            .expect("fake ordered source")
            .renewals
            .get(&(l.collection_id, id))
            .cloned();
        let Some(state) = state else {
            return Ok(OrderedRenewalAbandon::Retired {
                tail_uncertain: false,
            });
        };
        if state.old != old
            || new.is_some_and(|expected| state.new.is_some_and(|actual| actual != expected))
        {
            return Err(DesktopError::subscribe_failed("stale member renewal"));
        }
        if state.resumed {
            return Ok(OrderedRenewalAbandon::Retired {
                tail_uncertain: true,
            });
        }
        let Some(current) = new else {
            return Ok(OrderedRenewalAbandon::Pending);
        };
        if state.new != Some(current) || !state.rebound {
            return Ok(OrderedRenewalAbandon::Pending);
        }
        self.resume_ordered_member(l, id, current).await?;
        Ok(OrderedRenewalAbandon::Retired {
            tail_uncertain: true,
        })
    }
    fn rebind_ordered_member(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        old: ubm_core::ordered_notifications::Membership,
        new: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        let mut s = self.ordered.lock().expect("fake ordered source");
        let r = s
            .renewals
            .get(&(l.collection_id, id))
            .filter(|r| r.old == old && r.new == Some(new))
            .ok_or_else(|| DesktopError::subscribe_failed("stale renewal"))?;
        if r.rebound {
            return Ok(());
        }
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        s.registry
            .rebind_member(l.collection_id, &source, id, old, new)
            .map_err(|e| DesktopError::subscribe_failed(format!("member rebind: {e:?}")))?;
        let claim = s
            .claims
            .get_mut(&l.collection_id)
            .and_then(|v| v.iter_mut().find(|(i, _, _)| *i == id))
            .ok_or_else(|| DesktopError::subscribe_failed("missing claim"))?;
        claim.1 = new;
        let member = s
            .leases
            .get_mut(&l.collection_id)
            .and_then(|lease| lease.members.iter_mut().find(|(i, _)| *i == id))
            .ok_or_else(|| DesktopError::subscribe_failed("missing member"))?;
        member.1 = new;
        s.renewals
            .get_mut(&(l.collection_id, id))
            .expect("retained renewal")
            .rebound = true;
        Ok(())
    }
    async fn resume_ordered_member(
        &self,
        l: &OrderedSourceLease,
        id: u64,
        new: ubm_core::ordered_notifications::Membership,
    ) -> Result<(), DesktopError> {
        let mut s = self.ordered.lock().expect("fake ordered source");
        let r = s
            .renewals
            .get_mut(&(l.collection_id, id))
            .filter(|r| r.new == Some(new) && r.rebound)
            .ok_or_else(|| DesktopError::subscribe_failed("stale renewal"))?;
        if r.resumed {
            return Ok(());
        }
        r.resumed = true;
        if s.renewals
            .iter()
            .any(|((group, _), r)| *group == l.collection_id && !r.resumed)
        {
            return Ok(());
        }
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        if let Some(mut records) = s.buffered.remove(&l.collection_id) {
            while let Some(record) = records.pop_front() {
                match record {
                    crate::fake_ordered_source::FakeRecord::Value(m, o, v, p) => {
                        s.registry
                            .admit_value(
                                &source,
                                m,
                                o,
                                &v,
                                ubm_core::ordered_notifications::ObservedDelivery::Unknown,
                                p,
                            )
                            .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
                    }
                    crate::fake_ordered_source::FakeRecord::Loss(members, count) => {
                        s.registry
                            .admit_source_loss(&source, &members, count)
                            .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
                    }
                }
            }
        }
        if let Some(rejection) = s.overflows.remove(&l.collection_id) {
            let members = s
                .leases
                .get(&l.collection_id)
                .expect("lease")
                .members
                .iter()
                .map(|(_, m)| *m)
                .collect::<Vec<_>>();
            s.registry
                .source_terminal(
                    &source,
                    &members,
                    ubm_core::ordered_notifications::EndReason::SourceOverflow,
                    rejection,
                    None,
                    true,
                )
                .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
        }
        Ok(())
    }
    async fn poll_ordered_source(
        &self,
        l: &OrderedSourceLease,
    ) -> Result<ubm_core::ordered_notifications::Poll, DesktopError> {
        let mut s = self.ordered.lock().expect("fake ordered source");
        if !s.registry.owns_source(
            l.collection_id,
            &crate::fake_ordered_source::FakeOrderedSource::source(l),
        ) {
            return Err(DesktopError::subscribe_failed("stale lease"));
        }
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        s.flush_pending_records(&source)?;
        let result = s
            .registry
            .poll(l.collection_id)
            .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
        s.flush_pending_records(&source)?;
        Ok(result)
    }
    fn ordered_marker_write_supported(&self) -> bool {
        true
    }
    async fn start_ordered_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        {
            let mut state = self.ordered.lock().expect("fake ordered source");
            let key = (lease.collection_id, request.marker_id);
            if let Some((existing, status)) = state.marker_transactions.get(&key) {
                return if existing == request {
                    Ok(status.clone())
                } else {
                    Err(DesktopError::subscribe_failed("marker identity mismatch"))
                };
            }
            state
                .marker_transactions
                .insert(key, (request.clone(), OrderedMarkerWriteStatus::Pending));
        }
        self.record("ordered_marker_pending");
        self.gate(FaultOp::OrderedMarkerBeforeAdmission).await;
        let status = {
            let mut state = self.ordered.lock().expect("fake ordered source");
            let key = (lease.collection_id, request.marker_id);
            match state.marker_transactions.get(&key) {
                Some((existing, OrderedMarkerWriteStatus::Pending)) if existing == request => {}
                Some((existing, status)) if existing == request => return Ok(status.clone()),
                _ => {
                    return Err(DesktopError::subscribe_failed(
                        "marker start already retired",
                    ));
                }
            }
            let ordinal = *state.ordinals.entry(lease.peer_id.clone()).or_insert(0);
            let source = crate::fake_ordered_source::FakeOrderedSource::source(lease);
            let phase = state
                .registry
                .with_collection_silent(lease.collection_id, |owner| Ok(owner.phase()))
                .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))?;
            let status = if matches!(
                phase,
                ubm_core::ordered_notifications::Phase::Closing
                    | ubm_core::ordered_notifications::Phase::Terminal
            ) {
                OrderedMarkerWriteStatus::NotAdmitted(Some(DesktopError::subscribe_failed(
                    "source already terminated",
                )))
            } else if let Some(next) = ordinal.checked_add(1) {
                state.ordinals.insert(lease.peer_id.clone(), next);
                state
                    .registry
                    .admit_marker(&source, request.marker_id, ordinal)
                    .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))?;
                OrderedMarkerWriteStatus::Admitted {
                    ordinal,
                    write: OrderedMarkerWritePhase::Started(None),
                }
            } else {
                let members = lease
                    .members
                    .iter()
                    .map(|(_, member)| *member)
                    .collect::<Vec<_>>();
                state
                    .registry
                    .source_terminal(
                        &source,
                        &members,
                        ubm_core::ordered_notifications::EndReason::SourceOrdinalExhausted,
                        Some(ubm_core::ordered_notifications::SourceRejection::Marker {
                            marker_id: request.marker_id,
                        }),
                        None,
                        true,
                    )
                    .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))?;
                OrderedMarkerWriteStatus::Rejected(DesktopError::subscribe_failed(
                    "source ordinal exhausted",
                ))
            };
            state
                .marker_transactions
                .insert(key, (request.clone(), status.clone()));
            status
        };
        if matches!(status, OrderedMarkerWriteStatus::Admitted { .. }) {
            self.record("ordered_marker_start");
            self.gate(FaultOp::OrderedMarkerStart).await;
        }
        Ok(status)
    }
    async fn cancel_ordered_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let outcome = {
            let mut state = self.ordered.lock().expect("fake ordered source");
            let (existing, status) = state
                .marker_transactions
                .entry((lease.collection_id, request.marker_id))
                .or_insert_with(|| (request.clone(), OrderedMarkerWriteStatus::NotAdmitted(None)));
            if existing != request {
                return Err(DesktopError::subscribe_failed("marker identity mismatch"));
            }
            if matches!(status, OrderedMarkerWriteStatus::Pending) {
                *status = OrderedMarkerWriteStatus::NotAdmitted(None);
            }
            status.clone()
        };
        self.record("ordered_marker_cancel");
        self.gate(FaultOp::OrderedMarkerCancel).await;
        Ok(outcome)
    }
    async fn ordered_marker_write_status(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<OrderedMarkerWriteStatus, DesktopError> {
        let state = self.ordered.lock().expect("fake ordered source");
        match state
            .marker_transactions
            .get(&(lease.collection_id, request.marker_id))
        {
            Some((existing, status)) if existing == request => Ok(status.clone()),
            _ => Err(DesktopError::subscribe_failed("marker identity mismatch")),
        }
    }
    async fn forget_ordered_marker_write(
        &self,
        lease: &OrderedSourceLease,
        request: &OrderedMarkerWriteRequest,
    ) -> Result<(), DesktopError> {
        let mut state = self.ordered.lock().expect("fake ordered source");
        match state
            .marker_transactions
            .get(&(lease.collection_id, request.marker_id))
        {
            Some((
                existing,
                OrderedMarkerWriteStatus::Admitted {
                    write:
                        OrderedMarkerWritePhase::Settled(_) | OrderedMarkerWritePhase::NotStarted(_),
                    ..
                },
            )) if existing == request => {}
            Some((
                existing,
                OrderedMarkerWriteStatus::Rejected(_) | OrderedMarkerWriteStatus::NotAdmitted(_),
            )) if existing == request => {}
            _ => {
                return Err(DesktopError::subscribe_failed(
                    "marker transaction remains pending",
                ));
            }
        }
        state
            .marker_transactions
            .remove(&(lease.collection_id, request.marker_id));
        Ok(())
    }
    fn reserve_ordered_marker(&self, l: &OrderedSourceLease, id: u64) -> Result<(), DesktopError> {
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        self.ordered
            .lock()
            .expect("fake ordered source")
            .registry
            .reserve_marker(l.collection_id, &source, id)
            .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))
    }
    fn ordered_marker_credit_pending(
        &self,
        l: &OrderedSourceLease,
        id: u64,
    ) -> Result<bool, DesktopError> {
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        self.ordered
            .lock()
            .expect("fake ordered source")
            .registry
            .marker_reserved(l.collection_id, &source, id)
            .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))
    }
    fn release_ordered_marker(&self, l: &OrderedSourceLease, id: u64) -> Result<(), DesktopError> {
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        self.ordered
            .lock()
            .expect("fake ordered source")
            .registry
            .release_marker(l.collection_id, &source, id)
            .map_err(|error| DesktopError::subscribe_failed(format!("{error:?}")))
    }
    async fn fence_ordered_source(&self, l: &OrderedSourceLease) -> Result<(), DesktopError> {
        #[cfg(test)]
        if let Some(fault) = self.take_fault(FaultOp::OrderedSourceFence) {
            return Err(DesktopError::subscribe_failed(fault.detail));
        }
        let s = self.ordered.lock().expect("fake ordered source");
        if s.registry.owns_source(
            l.collection_id,
            &crate::fake_ordered_source::FakeOrderedSource::source(l),
        ) {
            Ok(())
        } else {
            Err(DesktopError::subscribe_failed("stale lease"))
        }
    }
    async fn retire_ordered_source(
        &self,
        l: &OrderedSourceLease,
        reason: ubm_core::ordered_notifications::EndReason,
    ) -> Result<(), DesktopError> {
        #[cfg(test)]
        if let Some(fault) = self.take_fault(FaultOp::OrderedSourceRetire) {
            return Err(DesktopError::subscribe_failed(fault.detail));
        }
        let mut s = self.ordered.lock().expect("fake ordered source");
        let source = crate::fake_ordered_source::FakeOrderedSource::source(l);
        s.flush_pending_records(&source)?;
        if s.buffered.contains_key(&l.collection_id) {
            return Err(DesktopError::subscribe_failed(
                "member renewal must settle before source retirement",
            ));
        }
        if !s.registry.owns_source(l.collection_id, &source) {
            return Err(DesktopError::subscribe_failed("stale lease"));
        }
        let members: Vec<_> = l.members.iter().map(|(_, m)| *m).collect();
        // Rejection or overflow may already have ended this exact owner. Its
        // terminal is authoritative; retirement only seals its remaining route.
        let phase = s
            .registry
            .with_collection_silent(l.collection_id, |owner| Ok(owner.phase()))
            .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
        if !matches!(
            phase,
            ubm_core::ordered_notifications::Phase::Closing
                | ubm_core::ordered_notifications::Phase::Terminal
        ) {
            s.registry
                .source_terminal(&source, &members, reason, None, None, true)
                .map_err(|e| DesktopError::subscribe_failed(format!("{e:?}")))?;
        }
        for (_, m) in &l.members {
            s.registry.retire_membership(&source, *m);
        }
        s.sealed.insert(l.collection_id);
        Ok(())
    }
    async fn seal_ordered_source(
        &self,
        l: &OrderedSourceLease,
    ) -> Result<OrderedSourceSeal, DesktopError> {
        let s = self.ordered.lock().expect("fake ordered source");
        if !s.registry.owns_source(
            l.collection_id,
            &crate::fake_ordered_source::FakeOrderedSource::source(l),
        ) {
            return Err(DesktopError::subscribe_failed("stale lease"));
        }
        Ok(if s.sealed.contains(&l.collection_id) {
            OrderedSourceSeal::Sealed
        } else {
            OrderedSourceSeal::Pending
        })
    }
    async fn release_ordered_source(
        &self,
        cleanup: &OrderedSourceCleanup,
    ) -> Result<OrderedSourceRelease, DesktopError> {
        let (peer, id) = match cleanup {
            OrderedSourceCleanup::Lease(l) => (l.peer_id.as_str(), l.collection_id),
            OrderedSourceCleanup::Indeterminate {
                peer_id,
                collection_id,
            } => (peer_id.as_str(), *collection_id),
        };
        let mut s = self.ordered.lock().expect("fake ordered source");
        if let Some(l) = s.leases.get(&id) {
            if l.peer_id != peer {
                return Err(DesktopError::subscribe_failed("stale peer"));
            }
        } else {
            return Ok(OrderedSourceRelease::Released);
        }
        if s.origins.iter().any(|(key, (collection, _, _))| {
            *collection == id
                && s.settlements.get(key) != Some(&OrderedNotificationSettlement::Released)
        }) {
            return Ok(OrderedSourceRelease::Pending);
        }
        match s.registry.remove(id) {
            Ok(()) => {}
            Err(crate::ordered_notification_groups::RegistryError::CleanupPending) => {
                return Ok(OrderedSourceRelease::Pending);
            }
            Err(e) => return Err(DesktopError::subscribe_failed(format!("{e:?}"))),
        }
        s.origins.retain(|_, (collection, _, _)| *collection != id);
        s.leases.remove(&id);
        s.claims.remove(&id);
        s.sealed.remove(&id);
        Ok(OrderedSourceRelease::Released)
    }
    #[cfg(target_os = "linux")]
    fn reserve_physical_loss(
        &self,
        peer: &str,
        generation: u64,
        reason: u8,
    ) -> PhysicalLossReservation {
        if let Some(fault) = self.take_fault(FaultOp::NotificationFence) {
            return PhysicalLossReservation::Indeterminate(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Gatt,
                    "gatt.notification.fence",
                )
                .with_detail(fault.detail),
            );
        }
        let mut s = self.state.lock().expect("fake radio state");
        if generation == 0 || s.physical_generations.get(peer) != Some(&generation) {
            return PhysicalLossReservation::Stale;
        }
        if s.physical_pins.contains_key(peer) {
            return PhysicalLossReservation::Indeterminate(DesktopError::subscribe_failed(
                "physical loss already pinned",
            ));
        }
        let Some(token) = s.physical_pin_serial.checked_add(1) else {
            return PhysicalLossReservation::Indeterminate(DesktopError::subscribe_failed(
                "physical pin exhausted",
            ));
        };
        s.physical_pin_serial = token;
        s.physical_pins.insert(peer.to_owned(), token);
        let guard = FakePhysicalLossGuard {
            state: Arc::clone(&self.state),
            peer: peer.to_owned(),
            token,
            active: std::sync::atomic::AtomicBool::new(true),
        };
        PhysicalLossReservation::Accepted(PhysicalLossPermit {
            peer_id: peer.to_owned(),
            generation,
            reason,
            token,
            guard: Some(Arc::new(guard)),
        })
    }
    #[cfg(target_os = "linux")]
    async fn fence_reserved_physical_loss(&self, p: &PhysicalLossPermit) -> PhysicalLossFence {
        let Some(guard) = p.guard.as_ref() else {
            return PhysicalLossFence::Stale;
        };
        let (ack, receiver) = PeerSourceFenceAck::new();
        let result = {
            let mut s = self.state.lock().expect("fake radio state");
            if s.physical_generations.get(&p.peer_id) != Some(&p.generation)
                || s.physical_pins.get(&p.peer_id) != Some(&p.token)
            {
                None
            } else if s.events_closed || s.control.len() >= FAKE_CONTROL_CAP {
                Some(false)
            } else {
                s.seq = s.seq.saturating_add(1);
                let seq = s.seq;
                s.control
                    .push_back((seq, RadioEvent::PeerNotificationSourceFence { ack }));
                Some(true)
            }
        };
        guard.release();
        match result {
            None => PhysicalLossFence::Stale,
            Some(false) => PhysicalLossFence::Accepted(Err(DesktopError::subscribe_failed(
                "physical loss fence unavailable",
            ))),
            Some(true) => {
                self.notify.notify_one();
                PhysicalLossFence::Accepted(receiver.await.map_err(|_| {
                    DesktopError::subscribe_failed("physical loss fence not acknowledged")
                }))
            }
        }
    }
    #[cfg(target_os = "linux")]
    async fn fence_physical_loss(
        &self,
        peer: &str,
        generation: u64,
        _reason: u8,
    ) -> PhysicalLossFence {
        if let Some(fault) = self.take_fault(FaultOp::NotificationFence) {
            return PhysicalLossFence::Indeterminate(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Gatt,
                    "gatt.notification.fence",
                )
                .with_detail(fault.detail),
            );
        }
        let (ack, receiver) = PeerSourceFenceAck::new();
        {
            let mut state = self.state.lock().expect("fake radio state");
            if generation == 0 || state.physical_generations.get(peer) != Some(&generation) {
                return PhysicalLossFence::Stale;
            }
            if state.events_closed || state.control.len() >= FAKE_CONTROL_CAP {
                return PhysicalLossFence::Accepted(Err(DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Gatt,
                    "gatt.notification.fence",
                )));
            }
            state.seq = state.seq.saturating_add(1);
            let seq = state.seq;
            state
                .control
                .push_back((seq, RadioEvent::PeerNotificationSourceFence { ack }));
        }
        self.notify.notify_one();
        PhysicalLossFence::Accepted(receiver.await.map_err(|_| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Gatt,
                "gatt.notification.fence",
            )
        }))
    }

    async fn flush_peer_notification_source(&self, _peer_id: &str) -> Result<(), DesktopError> {
        self.record("notification_fence");
        self.gate(FaultOp::NotificationFence).await;
        let (ack, receiver) = PeerSourceFenceAck::new();
        self.push_event(RadioEvent::PeerNotificationSourceFence { ack })?;
        receiver.await.map_err(|_| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Gatt,
                "gatt.notification.fence",
            )
        })
    }

    async fn acquire_gatt(
        &self,
        scope: &InstanceKey,
        kind: crate::acquired_gatt::AcquisitionKind,
    ) -> Result<crate::acquired_gatt::AcquiredGattTransport, DesktopError> {
        self.record(match kind {
            crate::acquired_gatt::AcquisitionKind::Write => "acquire_write",
            crate::acquired_gatt::AcquisitionKind::Notify => "acquire_notify",
        });
        self.acquired.acquire(scope, kind)
    }

    fn priority_capability_limitation(&self) -> Result<Option<&'static str>, DesktopError> {
        self.state
            .lock()
            .expect("fake radio state")
            .priority_capability
            .clone()
    }
    async fn request_priority(
        &self,
        peer_id: &str,
        priority: ConnectionPriority,
    ) -> Result<bool, DesktopError> {
        self.record("request_priority");
        self.gate(FaultOp::RequestPriority).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::RequestPriority)
        {
            return Err(scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::PlatformFailure,
                    ubm_core::contracts::BleErrorDomain::Platform,
                    "connection.request-priority",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.state
            .lock()
            .expect("fake radio state")
            .priority_requests
            .push((peer_id.to_owned(), priority));
        Ok(true)
    }
    fn connection_parameters_capability_limitation(
        &self,
    ) -> Result<Option<&'static str>, DesktopError> {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_parameters_capability
            .clone()
    }
    #[cfg(target_os = "linux")]
    async fn accept_physical_loss(&self, peer: &str, generation: u64, _reason: u8) -> bool {
        let mut state = self.state.lock().expect("fake radio state");
        if generation != 0 && state.physical_generations.get(peer) == Some(&generation) {
            state.physical_generations.remove(peer);
            true
        } else {
            false
        }
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    fn ordered_reclaim_requires_snapshot_binding(&self) -> bool {
        self.state
            .lock()
            .expect("fake radio state")
            .reclaim_binding_error
            .is_some()
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    fn bind_ordered_reclaim_snapshot(
        &self,
        _peer: &str,
        _connection: &str,
        _identity: &GattSnapshotIdentity,
    ) -> Result<(), DesktopError> {
        match self
            .state
            .lock()
            .expect("fake radio state")
            .reclaim_binding_error
            .clone()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn ordered_recording_reclaim_supported(&self) -> bool {
        cfg!(any(test, feature = "reclaim-test-support"))
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn open_ordered_reclaim_authority(&self, request: u64) -> Result<u64, DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        if state.reclaim_retired_requests.contains(&request) {
            return Err(DesktopError::subscribe_failed("retired authority request"));
        }
        let id = if let Some((id, closed)) = state.reclaim_authorities.get(&request) {
            if *closed {
                return Err(DesktopError::subscribe_failed("closed authority request"));
            }
            *id
        } else {
            if std::mem::take(&mut state.reclaim_authority_notdispatched) {
                return Err(DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                    ubm_core::contracts::BleErrorDomain::Cleanup,
                    "gatt.ordered.recording-reclaim",
                )
                .with_outcome(
                    Some(ubm_core::contracts::CommitState::NotDispatched),
                    crate::Retryability::CallerDecides,
                ));
            }
            if state.reclaim_authorities.len() >= 64 {
                return Err(DesktopError::subscribe_failed(
                    "synthetic authority capacity",
                ));
            }
            let id = state.reclaim_next_authority;
            state.reclaim_next_authority = id
                .checked_add(1)
                .ok_or_else(|| DesktopError::subscribe_failed("authority IDs exhausted"))?;
            state.reclaim_authorities.insert(request, (id, false));
            id
        };
        if std::mem::take(&mut state.reclaim_lost_open) {
            return Err(DesktopError::subscribe_failed("lost authority open reply"));
        }
        Ok(id)
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn forget_ordered_reclaim_open(&self, request: u64) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        if state.reclaim_authorities.contains_key(&request) {
            return Err(DesktopError::subscribe_failed("authority still exists"));
        }
        if state.reclaim_retired_requests.len() >= 256
            && !state.reclaim_retired_requests.contains(&request)
        {
            return Err(DesktopError::subscribe_failed(
                "synthetic retired-request capacity",
            ));
        }
        state.reclaim_retired_requests.insert(request);
        Ok(())
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn close_ordered_reclaim_authority(&self, authority: u64) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        let Some((_, closed)) = state
            .reclaim_authorities
            .values_mut()
            .find(|(id, _)| *id == authority)
        else {
            return Err(DesktopError::subscribe_failed("unknown authority"));
        };
        *closed = true;
        if std::mem::take(&mut state.reclaim_lost_close) {
            return Err(DesktopError::subscribe_failed("lost authority close reply"));
        }
        Ok(())
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn forget_ordered_reclaim_authority(&self, authority: u64) -> Result<(), DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        let Some(request) = state
            .reclaim_authorities
            .iter()
            .find_map(|(request, (id, closed))| (*id == authority && *closed).then_some(*request))
        else {
            return if state.reclaim_retired_authorities.contains(&authority) {
                Ok(())
            } else {
                Err(DesktopError::subscribe_failed("authority not closed"))
            };
        };
        if state.reclaim_retired_authorities.len() >= 256
            && !state.reclaim_retired_authorities.contains(&authority)
        {
            return Err(DesktopError::subscribe_failed(
                "synthetic retired-authority capacity",
            ));
        }
        if state.reclaim_retired_requests.len() >= 256
            && !state.reclaim_retired_requests.contains(&request)
        {
            return Err(DesktopError::subscribe_failed(
                "synthetic retired-request capacity",
            ));
        }
        state.reclaim_authorities.remove(&request);
        state.reclaim_retired_authorities.insert(authority);
        state.reclaim_next_permit.remove(&authority);
        state.reclaim_retired_permits.remove(&authority);
        state.reclaim_retired_requests.insert(request);
        Ok(())
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn reserve_ordered_reclaim_permit(
        &self,
        request: &OrderedReclaimPermitRequest,
    ) -> Result<u64, DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        let authority = request.authority_id;
        let key = (authority, request.request_id);
        let id = if let Some((held, id)) = state.reclaim_permit_receipts.get(&key) {
            if held != request {
                return Err(DesktopError::subscribe_failed("conflicting permit request"));
            }
            *id
        } else {
            if std::mem::take(&mut state.reclaim_permit_capacity_refusal) {
                return Err(DesktopError::new(
                    ubm_core::contracts::BleErrorCode::LifecycleInvalidState,
                    ubm_core::contracts::BleErrorDomain::Cleanup,
                    "gatt.ordered.recording-reclaim",
                )
                .with_outcome(
                    Some(ubm_core::contracts::CommitState::NotDispatched),
                    crate::Retryability::CallerDecides,
                ));
            }
            // Simulate prior retired source receipts so host request IDs do not
            // accidentally equal the independently issued native IDs.
            let next = state.reclaim_next_permit.entry(authority).or_insert(101);
            let id = *next;
            *next = id
                .checked_add(1)
                .ok_or_else(|| DesktopError::subscribe_failed("permit IDs exhausted"))?;
            state
                .reclaim_permit_receipts
                .insert(key, (request.clone(), id));
            id
        };
        if std::mem::take(&mut state.reclaim_lost_permit_reserve) {
            return Err(DesktopError::subscribe_failed(
                "lost permit reservation reply",
            ));
        }
        Ok(id)
    }
    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn forget_ordered_reclaim_permit_request(
        &self,
        _request: &OrderedReclaimPermitRequest,
    ) -> Result<(), DesktopError> {
        Ok(())
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn acquire_ordered_reclaim_graph(
        &self,
        key: &OrderedReclaimGraphKey,
    ) -> Result<OrderedReclaimGraphAcquire, DesktopError> {
        self.record("reclaim_acquire");
        self.gate(FaultOp::ReclaimAcquire).await;
        let mut state = self.state.lock().expect("fake radio state");
        if let Some((held, _)) = state
            .reclaim_permits
            .get(&(key.authority_id, key.permit_id))
        {
            return if held == key {
                Ok(OrderedReclaimGraphAcquire::Acquired)
            } else {
                Err(DesktopError::subscribe_failed("conflicting reclaim permit"))
            };
        }
        if state.gatt_identities.get(&key.peer_id) != Some(&key.identity) {
            return Ok(OrderedReclaimGraphAcquire::Stale);
        }
        state
            .reclaim_permits
            .insert((key.authority_id, key.permit_id), (key.clone(), false));
        if std::mem::take(&mut state.reclaim_lost_acquire) {
            return Err(DesktopError::subscribe_failed("lost acquire reply"));
        }
        Ok(OrderedReclaimGraphAcquire::Acquired)
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn commit_ordered_reclaim_graph(
        &self,
        key: &OrderedReclaimGraphKey,
    ) -> Result<OrderedReclaimGraphCommit, DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        if state
            .reclaim_permits
            .get(&(key.authority_id, key.permit_id))
            .is_some_and(|(held, committed)| held == key && *committed)
        {
            return Ok(OrderedReclaimGraphCommit::Committed);
        }
        if state.gatt_identities.get(&key.peer_id) != Some(&key.identity) {
            return Ok(OrderedReclaimGraphCommit::Invalidated(
                DesktopError::subscribe_failed("native graph invalidated"),
            ));
        }
        match state
            .reclaim_permits
            .get_mut(&(key.authority_id, key.permit_id))
        {
            Some((held, committed)) if held == key => {
                *committed = true;
                if std::mem::take(&mut state.reclaim_lost_commit) {
                    return Err(DesktopError::subscribe_failed("lost commit reply"));
                }
                Ok(OrderedReclaimGraphCommit::Committed)
            }
            _ => Err(DesktopError::subscribe_failed("unknown reclaim permit")),
        }
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn release_ordered_reclaim_graph(
        &self,
        key: &OrderedReclaimGraphKey,
    ) -> Result<(), DesktopError> {
        self.record("reclaim_release");
        self.gate(FaultOp::ReclaimRelease).await;
        #[cfg(test)]
        if let Some(fault) = self.take_fault(FaultOp::ReclaimRelease) {
            return Err(DesktopError::subscribe_failed(&fault.detail));
        }
        let mut state = self.state.lock().expect("fake radio state");
        if state
            .reclaim_permits
            .get(&(key.authority_id, key.permit_id))
            .is_some_and(|(held, _)| held != key)
        {
            return Err(DesktopError::subscribe_failed("conflicting reclaim permit"));
        }
        state
            .reclaim_permits
            .remove(&(key.authority_id, key.permit_id));
        if std::mem::take(&mut state.reclaim_lost_release) {
            return Err(DesktopError::subscribe_failed("lost release reply"));
        }
        Ok(())
    }

    #[cfg(any(test, feature = "reclaim-test-support"))]
    async fn forget_ordered_reclaim_graph(
        &self,
        key: &OrderedReclaimGraphKey,
    ) -> Result<(), DesktopError> {
        #[cfg(test)]
        {
            self.record("reclaim_forget");
            self.gate(FaultOp::ReclaimForget).await;
            if let Some(fault) = self.take_fault(FaultOp::ReclaimForget) {
                return Err(DesktopError::subscribe_failed(&fault.detail));
            }
        }
        let mut state = self.state.lock().expect("fake radio state");
        let request = state
            .reclaim_permit_receipts
            .iter()
            .find_map(|(token, (request, id))| {
                (request.authority_id == key.authority_id && *id == key.permit_id)
                    .then_some((*token, request.clone()))
            });
        if let Some((token, request)) = request {
            if request.peer_id != key.peer_id
                || request.connection_generation != key.connection_generation
                || request.identity != key.identity
            {
                return Err(DesktopError::subscribe_failed(
                    "permit receipt identity mismatch",
                ));
            }
            state.reclaim_permit_receipts.remove(&token);
            let (frontier, pending) = state
                .reclaim_retired_permits
                .entry(key.authority_id)
                .or_insert_with(|| (100, std::collections::BTreeSet::new()));
            pending.insert(key.permit_id);
            while let Some(next) = frontier.checked_add(1) {
                if !pending.remove(&next) {
                    break;
                }
                *frontier = next;
            }
            return Ok(());
        }
        if state
            .reclaim_retired_permits
            .get(&key.authority_id)
            .is_some_and(|(frontier, pending)| {
                key.permit_id <= *frontier || pending.contains(&key.permit_id)
            })
        {
            Ok(())
        } else {
            Err(DesktopError::subscribe_failed("unknown permit receipt"))
        }
    }

    fn gatt_snapshot_identity(
        &self,
        peer_id: &str,
    ) -> Result<Option<GattSnapshotIdentity>, DesktopError> {
        let mut state = self.state.lock().expect("fake radio state");
        if let Some(error) = state.gatt_identity_errors.get(peer_id) {
            return Err(error.clone());
        }
        if let Some(identity) = state
            .scripted_gatt_identities
            .get_mut(peer_id)
            .and_then(VecDeque::pop_front)
        {
            return Ok(identity);
        }
        Ok(state.gatt_identities.get(peer_id).cloned())
    }

    async fn peer_directory_capability_limitations(
        &self,
    ) -> Result<(Option<&'static str>, Option<&'static str>), DesktopError> {
        self.state
            .lock()
            .expect("fake radio state")
            .directory_capability_limitations
            .clone()
    }

    async fn known_directory_peers(&self) -> Result<Vec<DirectoryPeer>, DesktopError> {
        self.record("known_directory_peers");
        self.directory_gate().await;
        self.state
            .lock()
            .expect("fake radio state")
            .known_directory_peers
            .clone()
            .ok_or_else(|| unsupported("peers.known", "directory not scripted"))
    }

    async fn connected_peers(
        &self,
        _services: &[String],
    ) -> Result<Vec<DirectoryPeer>, DesktopError> {
        self.record("connected_peers");
        self.directory_gate().await;
        self.state
            .lock()
            .expect("fake radio state")
            .directory_peers
            .clone()
            .ok_or_else(|| unsupported("peers.connected", "directory not scripted"))
    }

    async fn bonded_peers(&self) -> Result<Vec<DirectoryPeer>, DesktopError> {
        self.record("bonded_peers");
        self.directory_gate().await;
        self.state
            .lock()
            .expect("fake radio state")
            .bonded_directory_peers
            .clone()
            .ok_or_else(|| unsupported("peers.bonded", "bonded directory not scripted"))
    }

    async fn resolve_peer(&self, peer_id: &str) -> Result<Option<DirectoryPeer>, DesktopError> {
        self.record("resolve_peer");
        self.directory_gate().await;
        let state = self.state.lock().expect("fake radio state");
        state
            .known_directory_peers
            .as_ref()
            .or(state.directory_peers.as_ref())
            .map(|peers| {
                peers
                    .iter()
                    .find(|peer| peer.peer_id == peer_id)
                    .cloned()
                    .map(|mut peer| {
                        peer.connection = "unknown";
                        peer
                    })
            })
            .ok_or_else(|| unsupported("peers.resolve", "directory not scripted"))
    }
    fn canonical_peer_id(&self, peer_id: &str) -> String {
        self.state
            .lock()
            .unwrap()
            .canonical_peer_ids
            .get(peer_id)
            .cloned()
            .unwrap_or_else(|| peer_id.to_owned())
    }
    async fn adapter_name(&self) -> Result<String, DesktopError> {
        self.record("adapter_name");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::AdapterName) {
            return Err(scripted(
                DesktopError::adapter_unavailable("adapter.name").with_detail(detail),
                platform,
            ));
        }
        Ok("fake-desktop-adapter".to_owned())
    }

    async fn start_scan(&self, filter: ScanFilterSpec) -> Result<(), DesktopError> {
        self.record("start_scan");
        self.state
            .lock()
            .expect("fake radio state")
            .scan_filters
            .push(filter);
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StartScan)` to hold a scan start in flight for
        // stop-while-starting and shutdown-during-start races.
        self.gate(FaultOp::StartScan).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::StartScan) {
            return Err(scripted(DesktopError::scan_start_failed(detail), platform));
        }
        self.state.lock().expect("fake radio state").scan_active = true;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.record("stop_scan");
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StopScan)` to hold a scan stop in flight.
        self.gate(FaultOp::StopScan).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::StopScan) {
            return Err(scripted(DesktopError::scan_stop_failed(detail), platform));
        }
        self.state.lock().expect("fake radio state").scan_active = false;
        Ok(())
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        self.record("peers");
        let peers = self
            .state
            .lock()
            .expect("fake radio state")
            .known_peers
            .clone();
        self.gate(FaultOp::PeerDirectory).await;
        Ok(peers)
    }

    async fn connect_when_available(
        &self,
        peer_id: &str,
        _expected_generation: &str,
    ) -> Result<(), DesktopError> {
        self.record("connect_when_available");
        self.connect(peer_id, _expected_generation).await
    }

    async fn connect(&self, peer_id: &str, _expected_generation: &str) -> Result<(), DesktopError> {
        self.record("connect");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Connect) {
            return Err(scripted(DesktopError::connection_failed(detail), platform));
        }
        self.gate(FaultOp::Connect).await;
        self.state
            .lock()
            .expect("fake radio state")
            .connected
            .push(peer_id.to_owned());
        Ok(())
    }

    async fn disconnect(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.record("disconnect");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Disconnect) {
            return Err(scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::ConnectionLost,
                    ubm_core::contracts::BleErrorDomain::Connection,
                    "connection.disconnect",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::Disconnect).await;
        self.state
            .lock()
            .expect("fake radio state")
            .connected
            .retain(|peer| peer != peer_id);
        Ok(())
    }

    async fn disconnect_with_observation(
        &self,
        peer_id: &str,
    ) -> Result<DisconnectObservation, DesktopError> {
        let platform = self
            .state
            .lock()
            .expect("fake radio state")
            .disconnect_observations
            .get(peer_id)
            .cloned();
        self.disconnect(peer_id).await?;
        Ok(DisconnectObservation {
            platform,
            physical_generation: None,
            cleanup_failure: self
                .state
                .lock()
                .expect("fake radio state")
                .disconnect_cleanup_failures
                .get(peer_id)
                .cloned(),
        })
    }
    fn consume_disconnect_observation(&self, peer: &str, observation: &DisconnectObservation) {
        let mut state = self.state.lock().expect("fake radio state");
        if state.disconnect_observations.get(peer) == observation.platform.as_ref() {
            state.disconnect_observations.remove(peer);
        }
    }

    async fn discover(&self, peer_id: &str) -> Result<Vec<ServiceSnapshot>, DesktopError> {
        self.record("discover");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Discover) {
            return Err(scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::GattDiscoveryRequired,
                    ubm_core::contracts::BleErrorDomain::Gatt,
                    "discovery.complete",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::Discover).await;
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .services
            .get(peer_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn read_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
    ) -> Result<CharacteristicRead, DesktopError> {
        self.record("read_characteristic");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Read) {
            return Err(scripted(DesktopError::read_failed(detail), platform));
        }
        self.gate(FaultOp::Read).await;
        let state = self.state.lock().expect("fake radio state");
        let value = state
            .values
            .get(&(
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ))
            .cloned()
            .unwrap_or_else(|| vec![0x42]);
        Ok(CharacteristicRead {
            value,
            provenance: state.read_provenance,
        })
    }

    async fn write_characteristic(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        self.record("write_characteristic");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Write) {
            return Err(scripted(DesktopError::write_failed(detail), platform));
        }
        self.gate(FaultOp::Write).await;
        let mut state = self.state.lock().expect("fake radio state");
        state.write_values.push(value);
        state.writes.push((
            (
                peer_id.to_owned(),
                service_uuid.to_owned(),
                service_occurrence,
                characteristic_uuid.to_owned(),
                characteristic_occurrence,
            ),
            with_response,
        ));
        Ok(())
    }

    async fn read_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
    ) -> Result<Vec<u8>, DesktopError> {
        self.record("read_descriptor");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Read) {
            return Err(scripted(DesktopError::read_failed(detail), platform));
        }
        self.gate(FaultOp::Read).await;
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_reads
            .push(descriptor_key(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                descriptor_uuid,
                descriptor_occurrence,
            ));
        Ok(vec![0x01])
    }

    async fn write_descriptor(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        descriptor_uuid: &str,
        descriptor_occurrence: u64,
        _value: Vec<u8>,
    ) -> Result<(), DesktopError> {
        self.record("write_descriptor");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Write) {
            return Err(scripted(DesktopError::write_failed(detail), platform));
        }
        self.gate(FaultOp::Write).await;
        self.state
            .lock()
            .expect("fake radio state")
            .descriptor_writes
            .push(descriptor_key(
                peer_id,
                service_uuid,
                service_occurrence,
                characteristic_uuid,
                characteristic_occurrence,
                descriptor_uuid,
                descriptor_occurrence,
            ));
        Ok(())
    }

    async fn set_notifications(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
        requested: Option<DeliveryMode>,
    ) -> Result<ObservedDelivery, DesktopError> {
        self.record("set_notifications");
        // Enable and disable faults inject independently: a CCCD-enable
        // refusal must not mask the disable-failure path under test.
        let fault = if enable {
            FaultOp::Subscribe
        } else {
            FaultOp::Unsubscribe
        };
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(fault) {
            return Err(scripted(DesktopError::subscribe_failed(detail), platform));
        }
        self.gate(fault).await;
        if !enable {
            let late_fault = self
                .state
                .lock()
                .expect("fake radio state")
                .late_disable_faults
                .pop_front();
            if let Some(detail) = late_fault {
                return Err(DesktopError::subscribe_failed(detail));
            }
        }
        let key = (
            peer_id.to_owned(),
            service_uuid.to_owned(),
            service_occurrence,
            characteristic_uuid.to_owned(),
            characteristic_occurrence,
        );
        let mut state = self.state.lock().expect("fake radio state");
        state
            .notifications
            .push((peer_id.to_owned(), characteristic_uuid.to_owned(), enable));
        if !enable {
            self.ordered
                .lock()
                .expect("fake ordered source")
                .settlements
                .insert(
                    (key.clone(), epoch),
                    OrderedNotificationSettlement::Released,
                );
            state.live_activations.remove(&(key.clone(), epoch));
            state.disable_epochs.push((key.clone(), epoch));
            if !state
                .live_activations
                .iter()
                .any(|(scope, _)| scope == &key)
            {
                state.live.remove(&key);
            }
            return Ok(ObservedDelivery::Unknown);
        }
        state.live.insert(key.clone());
        state.live_activations.insert((key.clone(), epoch));
        state.enable_epochs.push((key, epoch));
        state.delivery_requests.push(requested);
        let observed = state.scripted_delivery.unwrap_or(match requested {
            Some(DeliveryMode::Notification) => ObservedDelivery::Notification,
            Some(DeliveryMode::Indication) => ObservedDelivery::Indication,
            None => ObservedDelivery::Unknown,
        });
        Ok(observed)
    }

    async fn set_notifications_with_preference(
        &self,
        peer_id: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: &str,
        characteristic_occurrence: u64,
        enable: bool,
        epoch: u64,
        requested: Option<DeliveryMode>,
        preferred: Option<DeliveryMode>,
    ) -> Result<ObservedDelivery, DesktopError> {
        if enable {
            self.state
                .lock()
                .expect("fake radio state")
                .delivery_preferences
                .push(preferred);
        }
        // Scripted radio reports its own answer; preference is not an observed outcome.
        self.set_notifications(
            peer_id,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            enable,
            epoch,
            requested,
        )
        .await
    }

    async fn next_event(&self) -> Option<RadioEvent> {
        // F07: drain follows push order across the separate queues — earlier
        // data delivers before a later disconnect (causality preserved), yet
        // a full data queue never drops control.
        loop {
            {
                let mut state = self.state.lock().expect("fake radio state");
                let control_seq = state.control.front().map(|(seq, _)| *seq);
                let data_seq = state.data.front().map(|(seq, _)| *seq);
                let take_control = match (control_seq, data_seq) {
                    (Some(c), Some(d)) => c < d,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => {
                        if state.events_closed {
                            return None;
                        }
                        false
                    }
                };
                if control_seq.is_some() || data_seq.is_some() {
                    if take_control {
                        let (_, event) = state.control.pop_front().expect("control head");
                        return Some(event);
                    }
                    let (_, event) = state.data.pop_front().expect("data head");
                    if let RadioEvent::Notification { ref value, .. } = event {
                        let len = value.len() as u64;
                        state.data_bytes = state.data_bytes.saturating_sub(len);
                    }
                    return Some(event);
                }
            }
            self.notify.notified().await;
        }
    }

    async fn mtu(&self, peer_id: &str) -> Option<u16> {
        self.record("mtu");
        if self.take_fault(FaultOp::Mtu).is_some() {
            return None;
        }
        self.gate(FaultOp::Mtu).await;
        self.state
            .lock()
            .expect("fake radio state")
            .mtu
            .get(peer_id)
            .copied()
    }

    async fn close(&self) {
        self.record("close");
        // Deterministic scope order: HashSet iteration is unstable, and
        // close receipts must list scopes in a repeatable sequence.
        let mut scopes: Vec<InstanceKey> = self
            .state
            .lock()
            .expect("fake radio state")
            .live
            .iter()
            .cloned()
            .collect();
        scopes.sort();
        let mut state = self.state.lock().expect("fake radio state");
        state.close_failures.clear();
        for scope in scopes {
            // One queued `Unsubscribe` fault fails one scope's release (F14
            // injection): the failure is retained as a receipt and the
            // scope stays live, like an OS enablement that survived.
            let fault = state
                .faults
                .get_mut(&FaultOp::Unsubscribe)
                .and_then(VecDeque::pop_front);
            match fault {
                Some((detail, _)) => {
                    state
                        .close_failures
                        .push(RadioCloseFailure::new(scope, detail));
                }
                None => {
                    state.live.remove(&scope);
                    state.live_activations.retain(|(key, _)| key != &scope);
                }
            }
        }
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        std::mem::take(&mut self.state.lock().expect("fake radio state").close_failures)
    }

    async fn finish_close(&self) -> Vec<DesktopError> {
        self.record("finish_close");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::FinishClose) {
            return vec![scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::PlatformFailure,
                    ubm_core::contracts::BleErrorDomain::Cleanup,
                    "radio.close.transport",
                )
                .with_detail(detail),
                platform,
            )];
        }
        self.gate(FaultOp::FinishClose).await;
        let mut state = self.state.lock().expect("fake radio state");
        let retired: Vec<_> = state.disconnect_cleanup_retire_on_finish.drain().collect();
        for peer in retired {
            state.disconnect_cleanup_failures.remove(&peer);
        }
        state
            .disconnect_cleanup_failures
            .values()
            .cloned()
            .collect()
    }

    async fn read_rssi(&self, peer_id: &str) -> Result<i16, DesktopError> {
        self.record("read_rssi");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Rssi) {
            return Err(scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::PlatformFailure,
                    ubm_core::contracts::BleErrorDomain::Connection,
                    "peer.rssi",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::Rssi).await;
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .rssi
            .get(peer_id)
            .copied();
        scripted.ok_or_else(|| unsupported("peer.rssi", "no RSSI measured for this peer"))
    }

    async fn read_effective_mtu(&self, peer_id: &str) -> Result<Option<u16>, DesktopError> {
        self.record("read_effective_mtu");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::EffectiveMtu) {
            return Err(scripted(
                DesktopError::new(
                    ubm_core::contracts::BleErrorCode::PlatformFailure,
                    ubm_core::contracts::BleErrorDomain::Connection,
                    "connection.effective-mtu",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::EffectiveMtu).await;
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .effective_mtu
            .get(peer_id)
            .copied();
        Ok(scripted)
    }

    async fn adapter_state(&self) -> Result<AdapterPowerState, DesktopError> {
        self.record("adapter_state");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::AdapterState) {
            return Err(scripted(
                DesktopError::adapter_unavailable("adapter.state").with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::AdapterState).await;
        let scripted = self.state.lock().expect("fake radio state").adapter_state;
        scripted.ok_or_else(|| unsupported("adapter.state", "no adapter state scripted"))
    }

    fn admission_policy(&self) -> AdmissionPolicy {
        self.state.lock().expect("fake radio state").admission
    }

    fn tears_down_on_adapter_loss(&self) -> bool {
        self.state.lock().expect("fake radio state").teardown
    }

    fn os_answers_unflagged_subscribe(&self) -> bool {
        self.state
            .lock()
            .expect("fake radio state")
            .os_answers_unflagged_subscribe
    }

    fn ingress_notification_drops(&self) -> u64 {
        self.dropped_notification_count()
    }

    async fn adapter_authorization(&self) -> Result<AdapterAuthorization, DesktopError> {
        self.record("adapter_authorization");
        if let Some(ScriptedFault { detail, platform }) =
            self.take_fault(FaultOp::AdapterAuthorization)
        {
            return Err(scripted(
                DesktopError::adapter_unavailable("adapter.authorization").with_detail(detail),
                platform,
            ));
        }
        self.gate(FaultOp::AdapterAuthorization).await;
        let scripted = self.state.lock().expect("fake radio state").authorization;
        scripted.ok_or_else(|| {
            unsupported("adapter.authorization", "no adapter authorization scripted")
        })
    }

    async fn write_limits(&self, peer_id: &str) -> Option<WriteLimits> {
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .write_limits
            .get(peer_id)
            .copied();
        match scripted {
            Some(limits) => Some(limits),
            None => self.mtu(peer_id).await.and_then(WriteLimits::from_mtu),
        }
    }

    async fn observed_write_limit(
        &self,
        peer_id: &str,
        with_response: bool,
    ) -> Result<u16, DesktopError> {
        let withheld = self
            .state
            .lock()
            .expect("fake radio state")
            .unobserved_write_limits
            .contains(&(peer_id.to_owned(), with_response));
        if withheld {
            return Err(unobserved_write_limit(
                "scripted: the admission bound for this mode is not an OS observation",
            ));
        }
        observed_write_limit_of(self.write_limits(peer_id).await, with_response)
    }

    async fn security_state(&self, peer_id: &str) -> Result<SecurityState, DesktopError> {
        self.record("security_state");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::SecurityState) {
            return Err(scripted(
                fake_security_failure("security.state", detail),
                platform,
            ));
        }
        self.gate(FaultOp::SecurityState).await;
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .security
            .get(peer_id)
            .copied();
        scripted.ok_or_else(|| unsupported("security.state", "no security state scripted"))
    }

    async fn pair(&self, peer_id: &str) -> Result<PairOutcome, DesktopError> {
        self.record("pair");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Pair) {
            return Err(scripted(
                fake_security_failure("security.pair", detail),
                platform,
            ));
        }
        self.gate(FaultOp::Pair).await;
        let mut state = self.state.lock().expect("fake radio state");
        if state.cancelled_pairings.remove(peer_id) {
            return Ok(PairOutcome::Cancelled);
        }
        // Unscripted, the fake answers as a real OS does (finding 91): a
        // peer that is already bonded runs no ceremony and reports
        // `AlreadyPaired` with its current state; otherwise the ceremony
        // bonds it.
        let bonded = state
            .security
            .get(peer_id)
            .copied()
            .filter(|security| security.bond == BondState::Bonded);
        let outcome = state.pair_outcomes.remove(peer_id).unwrap_or_else(|| {
            bonded.map_or(
                PairOutcome::Paired(SecurityState {
                    bond: BondState::Bonded,
                    pairing_possible: Some(true),
                }),
                PairOutcome::AlreadyPaired,
            )
        });
        if let PairOutcome::Paired(security) | PairOutcome::AlreadyPaired(security) = &outcome {
            state.security.insert(peer_id.to_owned(), *security);
        }
        Ok(outcome)
    }

    async fn cancel_pairing(&self, peer_id: &str) -> Result<(), DesktopError> {
        self.record("cancel_pairing");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::CancelPairing) {
            return Err(scripted(
                fake_security_failure("security.cancel-pairing", detail),
                platform,
            ));
        }
        self.gate(FaultOp::CancelPairing).await;
        self.state
            .lock()
            .expect("fake radio state")
            .cancelled_pairings
            .insert(peer_id.to_owned());
        // A held ceremony observes the cancellation as soon as it resumes.
        self.unblock_op(FaultOp::Pair);
        Ok(())
    }

    async fn unpair(&self, peer_id: &str) -> Result<UnpairOutcome, DesktopError> {
        self.record("unpair");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Unpair) {
            return Err(scripted(
                fake_security_failure("security.unpair", detail),
                platform,
            ));
        }
        self.gate(FaultOp::Unpair).await;
        let mut state = self.state.lock().expect("fake radio state");
        let bonded = state
            .security
            .get(peer_id)
            .is_some_and(|security| security.bond == BondState::Bonded);
        state.security.insert(
            peer_id.to_owned(),
            SecurityState {
                bond: BondState::NotBonded,
                pairing_possible: Some(true),
            },
        );
        Ok(if bonded {
            UnpairOutcome::Unpaired
        } else {
            UnpairOutcome::AlreadyUnpaired
        })
    }

    async fn resolve_address(
        &self,
        address: &str,
        address_type: AddressType,
    ) -> Result<String, DesktopError> {
        self.record("resolve_address");
        self.gate(FaultOp::ResolveAddress).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::ResolveAddress) {
            return Err(scripted(
                DesktopError::adapter_unavailable("peer.address-targeting").with_detail(detail),
                platform,
            ));
        }
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .addresses
            .get(&(address.to_owned(), address_type))
            .cloned();
        scripted
            .ok_or_else(|| unsupported("peer.address-targeting", "no address resolution scripted"))
    }

    async fn address_type(&self, peer_id: &str) -> Result<Option<AddressType>, DesktopError> {
        self.record("address_type");
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .address_types
            .get(peer_id)
            .copied())
    }

    fn connection_phy_capability_limitation(&self) -> Result<Option<&'static str>, DesktopError> {
        self.state
            .lock()
            .expect("fake radio state")
            .connection_phy_capability
            .clone()
    }
    async fn release_terminal_resources(&self, _peer: &str) -> Result<(), DesktopError> {
        self.record("release_terminal_resources");
        self.gate(FaultOp::TerminalCleanup).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::TerminalCleanup)
        {
            return Err(scripted(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Cleanup,
                    "connection.retired-native-cleanup",
                )
                .with_detail(detail),
                platform,
            ));
        }
        Ok(())
    }
    async fn connection_phy(&self, peer_id: &str) -> Result<ObservedConnectionPhy, DesktopError> {
        self.record("connection_phy");
        self.gate(FaultOp::ConnectionPhy).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::ConnectionPhy) {
            return Err(scripted(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Platform,
                    "connection.phy",
                )
                .with_detail(detail),
                platform,
            ));
        }
        self.state
            .lock()
            .expect("fake radio state")
            .connection_phy
            .get(peer_id)
            .copied()
            .ok_or_else(|| unsupported("connection.phy", "no scripted PHY observation"))
    }
    async fn connection_parameters(
        &self,
        peer_id: &str,
    ) -> Result<ObservedConnectionParameters, DesktopError> {
        self.record("connection_parameters");
        self.gate(FaultOp::ConnectionParameters).await;
        if let Some(ScriptedFault { detail, platform }) =
            self.take_fault(FaultOp::ConnectionParameters)
        {
            return Err(scripted(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Platform,
                    "connection.parameters",
                )
                .with_detail(detail),
                platform,
            ));
        }
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .connection_parameters
            .get(peer_id)
            .copied();
        scripted.ok_or_else(|| {
            unsupported("connection.parameters", "no connection parameters scripted")
        })
    }

    async fn write_without_response_ready(&self, peer_id: &str) -> Result<bool, DesktopError> {
        self.record("write_without_response_ready");
        self.gate(FaultOp::WriteReadiness).await;
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::WriteReadiness) {
            return Err(scripted(
                DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Platform,
                    "gatt.write-readiness",
                )
                .with_detail(detail),
                platform,
            ));
        }
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .write_readiness
            .get(peer_id)
            .copied();
        scripted.ok_or_else(|| unsupported("gatt.write-readiness", "no readiness scripted"))
    }

    fn read_peer_notification_failure_recovery(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        offset: u64,
    ) -> impl std::future::Future<
        Output = Result<ubm_core::oversized_evidence::PeerSourceFailureChunk, DesktopError>,
    > + Send
    + '_ {
        self.record("peer_failure_recovery_read");
        let result = (|| {
            let state = self.state.lock().expect("fake radio state");
            let recovery = state
                .peer_failure_recovery
                .as_ref()
                .filter(|recovery| recovery.descriptor == *descriptor && !recovery.retired)
                .ok_or_else(|| {
                    DesktopError::new(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        "gatt.notification.source-failure-recovery.read",
                    )
                })?;
            let start = usize::try_from(offset).map_err(|_| {
                DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.notification.source-failure-recovery.read",
                )
            })?;
            if start > recovery.bytes.len() {
                return Err(DesktopError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Gatt,
                    "gatt.notification.source-failure-recovery.read",
                ));
            }
            let end = start
                .saturating_add(descriptor.chunk_bytes as usize)
                .min(recovery.bytes.len());
            Ok(ubm_core::oversized_evidence::PeerSourceFailureChunk {
                offset,
                next_offset: end as u64,
                bytes: recovery.bytes[start..end].to_vec(),
                done: end == recovery.bytes.len(),
            })
        })();
        async move {
            self.gate(FaultOp::PeerFailureRecoveryRead).await;
            result
        }
    }

    fn prepare_peer_notification_failure_recovery_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
    ) -> impl std::future::Future<Output = Result<String, DesktopError>> + Send + '_ {
        self.record("peer_failure_recovery_prepare");
        let mut state = self.state.lock().expect("fake radio state");
        let result = state
            .peer_failure_recovery
            .as_mut()
            .filter(|recovery| recovery.descriptor == *descriptor)
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.notification.source-failure-recovery.prepare-ack",
                )
            })
            .and_then(|recovery| {
                if recovery.prepare_failures_remaining > 0 {
                    recovery.prepare_failures_remaining -= 1;
                    return Err(DesktopError::new(
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Gatt,
                        "gatt.notification.source-failure-recovery.prepare-ack",
                    ));
                }
                Ok(recovery.source_ack_id.clone())
            });
        async move { result }
    }

    fn begin_peer_notification_failure_recovery_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> impl std::future::Future<Output = Result<String, DesktopError>> + Send + '_ {
        self.record("peer_failure_recovery_begin");
        let mut state = self.state.lock().expect("fake radio state");
        let result = state
            .peer_failure_recovery
            .as_mut()
            .filter(|recovery| {
                recovery.descriptor == *descriptor && recovery.source_ack_id == source_ack_id
            })
            .ok_or_else(|| {
                DesktopError::new(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    "gatt.notification.source-failure-recovery.begin-ack",
                )
            })
            .map(|recovery| {
                recovery.begun = true;
                recovery.source_ack_id.clone()
            });
        async move { result }
    }

    fn retire_peer_notification_failure_recovery_ack(
        &self,
        descriptor: &ubm_core::oversized_evidence::PeerSourceFailureDescriptor,
        source_ack_id: &str,
    ) -> impl std::future::Future<Output = Result<(), DesktopError>> + Send + '_ {
        self.record("peer_failure_recovery_retire");
        let mut state = self.state.lock().expect("fake radio state");
        let result = (|| {
            let recovery = state
                .peer_failure_recovery
                .as_mut()
                .filter(|recovery| {
                    recovery.descriptor == *descriptor
                        && recovery.source_ack_id == source_ack_id
                        && recovery.begun
                })
                .ok_or_else(|| {
                    DesktopError::new(
                        BleErrorCode::GattStaleHandle,
                        BleErrorDomain::Gatt,
                        "gatt.notification.source-failure-recovery.retire-ack",
                    )
                })?;
            if recovery.retire_failures_remaining > 0 {
                recovery.retire_failures_remaining -= 1;
                return Err(DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Gatt,
                    "gatt.notification.source-failure-recovery.retire-ack",
                ));
            }
            recovery.retired = true;
            Ok(())
        })();
        async move { result }
    }

    async fn characteristic_access(
        &self,
        peer_id: &str,
    ) -> Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError> {
        self.record("characteristic_access");
        #[cfg(test)]
        self.gate(FaultOp::DiscoveryAccess).await;
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .access
            .get(peer_id)
            .cloned();
        scripted.ok_or_else(|| {
            unsupported(
                "gatt.characteristic-access",
                "no characteristic facts scripted",
            )
        })
    }
}

/// A scripted security failure: `platform.security`, the frozen identity
/// the OS adapters use for a refused security call.
fn fake_security_failure(operation: &str, detail: String) -> DesktopError {
    DesktopError::new(
        ubm_core::contracts::BleErrorCode::PlatformSecurity,
        ubm_core::contracts::BleErrorDomain::Platform,
        operation,
    )
    .with_detail(detail)
}

#[cfg(test)]
mod tests {
    use super::{
        AdvertisementExtras, DeliveryMode, FakeRadio, FaultOp, ObservedDelivery, PeerSnapshot,
        PropertyFlags, RadioBoundary, RadioEvent,
    };

    #[tokio::test]
    async fn scripted_peers_are_listed() {
        let radio = FakeRadio::new();
        assert!(radio.peers().await.expect("listing").is_empty());
        let peer = PeerSnapshot {
            id: "peer-1".to_owned(),
            address: None,
            service_uuids: Vec::new(),
            rssi: Some(-40),
            local_name: Some("strap".to_owned()),
            manufacturer_data: Vec::new(),
            service_data: Vec::new(),
            tx_power_level: None,
            extras: AdvertisementExtras::default(),
        };
        radio.set_peers(vec![peer.clone()]);
        assert_eq!(radio.peers().await.expect("listing"), vec![peer]);
    }

    #[test]
    fn an_os_long_write_bounds_only_the_command() {
        assert_eq!(
            super::WriteLimits::os_long_write(super::ATT_DEFAULT_LE_MTU),
            Some(super::WriteLimits {
                with_response: 512,
                without_response: 20,
            })
        );
        assert_eq!(
            super::WriteLimits::os_long_write(247),
            Some(super::WriteLimits {
                with_response: 512,
                without_response: 244,
            })
        );
        assert_eq!(
            super::WriteLimits::os_long_write(517).map(|limits| limits.without_response),
            Some(512),
            "no single write carries more than one attribute value"
        );
        assert_eq!(super::WriteLimits::os_long_write(3), None);
    }

    /// Finding 91: like a real OS, pairing an already-bonded peer runs no
    /// ceremony and answers `AlreadyPaired` with the current state.
    #[tokio::test]
    async fn pairing_a_bonded_peer_answers_already_paired() {
        let radio = FakeRadio::new();
        let bonded = super::SecurityState {
            bond: super::BondState::Bonded,
            pairing_possible: Some(true),
        };
        assert_eq!(
            radio.pair("peer-1").await.expect("first"),
            super::PairOutcome::Paired(bonded)
        );
        assert_eq!(
            radio.pair("peer-1").await.expect("second"),
            super::PairOutcome::AlreadyPaired(bonded)
        );
        let scripted = super::SecurityState {
            bond: super::BondState::Bonded,
            pairing_possible: Some(false),
        };
        radio.set_security("peer-2", scripted);
        assert_eq!(
            radio.pair("peer-2").await.expect("scripted bond"),
            super::PairOutcome::AlreadyPaired(scripted)
        );
    }

    #[tokio::test]
    async fn injected_faults_carry_contract_identities() {
        let radio = FakeRadio::new();
        radio.fail_next(FaultOp::StartScan, "os denied");
        let error = radio
            .start_scan(super::ScanFilterSpec::default())
            .await
            .expect_err("scripted scan failure");
        assert_eq!(error.code_str(), "scan.start-failed");
        assert_eq!(radio.calls(), vec!["start_scan".to_owned()]);
        assert!(!radio.scan_active(), "failed start leaves scan inactive");
    }

    #[tokio::test]
    async fn close_releases_all_live_subscriptions() {
        let radio = FakeRadio::new();
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0, None)
            .await
            .expect("enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0, None)
            .await
            .expect("enable 1");
        assert_eq!(radio.live_subscription_count(), 2, "two live CCCDs");
        radio.close().await;
        assert_eq!(
            radio.live_subscription_count(),
            0,
            "teardown releases every live CCCD"
        );
        // Disabling one instance leaves the other live.
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0, None)
            .await
            .expect("re-enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0, None)
            .await
            .expect("re-enable 1");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, false, 0, None)
            .await
            .expect("disable 0");
        assert_eq!(
            radio.live_subscription_count(),
            1,
            "per-instance disable releases only its own CCCD"
        );
    }

    #[tokio::test]
    async fn f14_close_retains_per_scope_receipts() {
        let radio = FakeRadio::new();
        radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0, None)
            .await
            .expect("enable 0");
        radio
            .set_notifications("peer-1", "svc", 0, "char", 1, true, 0, None)
            .await
            .expect("enable 1");
        // One queued unsubscribe fault fails exactly one scope's release.
        radio.fail_next(FaultOp::Unsubscribe, "os stuck");
        radio.close().await;
        let failures = radio.take_close_failures();
        assert_eq!(failures.len(), 1, "exactly one close receipt");
        assert_eq!(
            failures[0].scope,
            (
                "peer-1".to_owned(),
                "svc".to_owned(),
                0,
                "char".to_owned(),
                0
            ),
            "failed scope is the first in deterministic order"
        );
        assert_eq!(failures[0].detail, "os stuck");
        assert_eq!(
            radio.live_subscription_count(),
            1,
            "failed scope stays live like a surviving OS enablement"
        );
        assert!(
            radio.take_close_failures().is_empty(),
            "receipts drain exactly once"
        );
    }

    #[tokio::test]
    async fn delivery_requirement_reaches_the_radio_and_the_observation_returns() {
        let radio = FakeRadio::new();
        let observed = radio
            .set_notifications("peer-1", "svc", 0, "char", 0, true, 0, None)
            .await
            .expect("enable without requirement");
        assert_eq!(
            observed,
            ObservedDelivery::Unknown,
            "no requirement, nothing observed"
        );
        let observed = radio
            .set_notifications(
                "peer-1",
                "svc",
                0,
                "char",
                1,
                true,
                0,
                Some(DeliveryMode::Indication),
            )
            .await
            .expect("enable with requirement");
        assert_eq!(
            observed,
            ObservedDelivery::Indication,
            "the fake writes the requested mode"
        );
        assert_eq!(
            radio.delivery_requests(),
            vec![None, Some(DeliveryMode::Indication)],
            "every enable records the requirement it carried"
        );
        radio.script_observed_delivery(ObservedDelivery::Notification);
        let observed = radio
            .set_notifications(
                "peer-1",
                "svc",
                0,
                "char",
                2,
                true,
                0,
                Some(DeliveryMode::Indication),
            )
            .await
            .expect("scripted observation");
        assert_eq!(
            observed,
            ObservedDelivery::Notification,
            "a scripted observation wins"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discover_gate_holds_the_call_until_released() {
        let radio = std::sync::Arc::new(FakeRadio::new());
        radio.block_op(FaultOp::Discover);
        let pending = tokio::spawn({
            let radio = std::sync::Arc::clone(&radio);
            async move { radio.discover("peer-1").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!pending.is_finished(), "discover waits on its gate");
        radio.unblock_op(FaultOp::Discover);
        pending.await.expect("join").expect("discover completes");
    }

    #[test]
    fn property_flags_map_to_core_bits() {
        let bits = PropertyFlags {
            read: true,
            write: false,
            write_without_response: true,
            notify: true,
            indicate: false,
        }
        .core_bits();
        assert_eq!(
            bits,
            ubm_core::central::GATT_PROP_READ
                | ubm_core::central::GATT_PROP_WRITE_NO_RESPONSE
                | ubm_core::central::GATT_PROP_NOTIFY
        );
    }

    #[tokio::test]
    async fn events_replay_in_order_then_close() {
        let radio = FakeRadio::new();
        radio
            .push_event(RadioEvent::Connected("peer-1".to_owned()))
            .expect("synthetic event admission failed");
        radio
            .push_event(RadioEvent::Disconnected("peer-1".to_owned()))
            .expect("synthetic event admission failed");
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::Connected("peer-1".to_owned()))
        );
        assert_eq!(
            radio.next_event().await,
            Some(RadioEvent::Disconnected("peer-1".to_owned()))
        );
        radio.close_events();
        assert_eq!(
            radio.next_event().await,
            None,
            "closed source ends the stream"
        );
    }
}
