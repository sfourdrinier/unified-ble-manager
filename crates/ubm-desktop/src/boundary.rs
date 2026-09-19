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

use crate::errors::DesktopError;

/// Which radio operation a scripted fault targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultOp {
    StartScan,
    StopScan,
    Connect,
    Disconnect,
    Discover,
    Read,
    Write,
    Subscribe,
    Unsubscribe,
    /// OS-reported ATT MTU lookup (`mtu()` returns `None`, as withheld).
    Mtu,
    /// Adapter identity readout (`adapter_name()` fails).
    AdapterName,
    /// Connected RSSI read (`read_rssi()` fails or holds).
    Rssi,
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
}

impl Default for ScanFilterSpec {
    /// No service or name filter, every advertisement.
    fn default() -> Self {
        Self {
            service_uuids: Vec::new(),
            duplicates: ubm_core::central::ScanDuplicatePolicy::All,
            name_prefix: None,
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

/// One service snapshot with occurrence index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub uuid: String,
    pub occurrence: u64,
    pub characteristics: Vec<CharacteristicSnapshot>,
}

/// Radio-side events delivered to the central event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioEvent {
    Advertisement(PeerSnapshot),
    Connected(String),
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
    /// The OS notification broadcast outran one subscription's receiver
    /// (vendored btleplug patch 10): `lost` notifications of the peer were
    /// missed, any of which may have been this instance's. Accounted on the
    /// subscription by its overflow policy.
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
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    fn disconnect<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<(), DesktopError>> + Send + 'a;
    fn discover<'a>(
        &'a self,
        peer_id: &'a str,
    ) -> impl Future<Output = Result<Vec<ServiceSnapshot>, DesktopError>> + Send + 'a;
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
    /// OS-reported ATT MTU for one peer, or `None` when the OS withholds
    /// it. An unmeasured MTU is never defaulted: the adapter fails writes
    /// closed with `capability.unavailable` instead of guessing 23.
    fn mtu<'a>(&'a self, peer_id: &'a str) -> impl Future<Output = Option<u16>> + Send + 'a;
    /// Next radio event, or `None` when the event source closes (scan
    /// cleanup path). Cancellation is owned by the caller: drop/stop the
    /// future rather than expecting the boundary to abort it.
    fn next_event(&self) -> impl Future<Output = Option<RadioEvent>> + Send + '_;
    /// Teardown hook: abort live notification forwarders and best-effort
    /// release OS-side CCCDs. Wired into central shutdown so no live OS
    /// subscription outlives the central. Infallible by contract: per-scope
    /// release failures are retained, never raised — the host drains them
    /// via [`RadioBoundary::take_close_failures`] into the shutdown report.
    fn close(&self) -> impl Future<Output = ()> + Send + '_;
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
    state: StdMutex<FakeInner>,
    notify: Arc<Notify>,
}

/// One characteristic instance address: (peer, service uuid, service
/// occurrence, characteristic uuid, characteristic occurrence).
pub type InstanceKey = (String, String, u64, String, u64);

/// One descriptor address: characteristic instance plus descriptor
/// uuid/occurrence.
pub type DescriptorKey = (InstanceKey, String, u64);

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
    faults: HashMap<FaultOp, VecDeque<(String, Option<crate::errors::PlatformDetail>)>>,
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
    /// Close-time release failures retained by the last [`FakeRadio::close`]
    /// (F14 receipts): one entry per scope whose unsubscribe fault fired.
    /// Drained by `take_close_failures`; failed scopes stay in `live`.
    close_failures: Vec<RadioCloseFailure>,
    /// Observed characteristic writes: addressed instance plus the
    /// response mode the adapter selected (`true` = with-response).
    writes: Vec<(InstanceKey, bool)>,
    /// Observed descriptor reads/writes: addressed descriptor keys.
    descriptor_reads: Vec<DescriptorKey>,
    descriptor_writes: Vec<DescriptorKey>,
    /// Subscription epochs captured at forwarder install, in enable
    /// order: addressed instance plus the epoch the central passed.
    enable_epochs: Vec<(InstanceKey, u64)>,
    /// Delivery requirement carried by each enable, in enable order.
    delivery_requests: Vec<Option<DeliveryMode>>,
    /// Scripted connected RSSI per peer (unset: unmeasured).
    rssi: HashMap<String, i16>,
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
    /// Scripted address resolutions: (address, type) -> peer id.
    addresses: HashMap<(String, AddressType), String>,
    /// Scripted per-peer address types.
    address_types: HashMap<String, AddressType>,
    /// Scripted per-peer characteristic facts (unset peer: unsupported).
    access: HashMap<String, HashMap<InstanceKey, CharacteristicAccess>>,
    /// Scripted per-peer write-without-response readiness (unset:
    /// unsupported).
    write_readiness: HashMap<String, bool>,
}

impl Default for FakeRadio {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRadio {
    pub fn new() -> Self {
        Self {
            state: StdMutex::new(FakeInner {
                faults: HashMap::new(),
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
                mtu: HashMap::new(),
                values: HashMap::new(),
                read_provenance: ReadProvenance::ReadResponse,
                live: HashSet::new(),
                close_failures: Vec::new(),
                writes: Vec::new(),
                descriptor_reads: Vec::new(),
                descriptor_writes: Vec::new(),
                enable_epochs: Vec::new(),
                delivery_requests: Vec::new(),
                rssi: HashMap::new(),
                adapter_state: None,
                scripted_delivery: None,
                gates: HashMap::new(),
                authorization: None,
                security: HashMap::new(),
                pair_outcomes: HashMap::new(),
                cancelled_pairings: HashSet::new(),
                write_limits: HashMap::new(),
                addresses: HashMap::new(),
                address_types: HashMap::new(),
                access: HashMap::new(),
                write_readiness: HashMap::new(),
            }),
            notify: Arc::new(Notify::new()),
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
    pub fn push_event(&self, event: RadioEvent) {
        let mut state = self.state.lock().expect("fake radio state");
        if state.events_closed {
            return;
        }
        let seq = state.seq.saturating_add(1);
        state.seq = seq;
        match event {
            RadioEvent::Notification { ref value, .. } => {
                let value_len = value.len() as u64;
                if state.data.len() >= FAKE_DATA_CAP
                    || state.data_bytes.saturating_add(value_len) > FAKE_DATA_BYTES
                {
                    state.dropped_data = state.dropped_data.saturating_add(1);
                    return;
                }
                state.data_bytes = state.data_bytes.saturating_add(value_len);
                state.data.push_back((seq, event));
            }
            control => {
                if state.control.len() >= FAKE_CONTROL_CAP {
                    state.dropped_control = state.dropped_control.saturating_add(1);
                    return;
                }
                state.control.push_back((seq, control));
            }
        }
        drop(state);
        self.notify.notify_one();
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

impl RadioBoundary for FakeRadio {
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
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::StartScan) {
            return Err(scripted(DesktopError::scan_start_failed(detail), platform));
        }
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StartScan)` to hold a scan start in flight for
        // stop-while-starting and shutdown-during-start races.
        self.gate(FaultOp::StartScan).await;
        self.state.lock().expect("fake radio state").scan_active = true;
        Ok(())
    }

    async fn stop_scan(&self) -> Result<(), DesktopError> {
        self.record("stop_scan");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::StopScan) {
            return Err(scripted(DesktopError::scan_stop_failed(detail), platform));
        }
        // Contention gate (mirrors connect/disconnect): tests close it via
        // `block_op(FaultOp::StopScan)` to hold a scan stop in flight.
        self.gate(FaultOp::StopScan).await;
        self.state.lock().expect("fake radio state").scan_active = false;
        Ok(())
    }

    async fn peers(&self) -> Result<Vec<PeerSnapshot>, DesktopError> {
        self.record("peers");
        Ok(self
            .state
            .lock()
            .expect("fake radio state")
            .known_peers
            .clone())
    }

    async fn connect(&self, peer_id: &str) -> Result<(), DesktopError> {
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
        _value: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DesktopError> {
        self.record("write_characteristic");
        if let Some(ScriptedFault { detail, platform }) = self.take_fault(FaultOp::Write) {
            return Err(scripted(DesktopError::write_failed(detail), platform));
        }
        self.gate(FaultOp::Write).await;
        self.state.lock().expect("fake radio state").writes.push((
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
            state.live.remove(&key);
            return Ok(ObservedDelivery::Unknown);
        }
        state.live.insert(key.clone());
        state.enable_epochs.push((key, epoch));
        state.delivery_requests.push(requested);
        let observed = state.scripted_delivery.unwrap_or(match requested {
            Some(DeliveryMode::Notification) => ObservedDelivery::Notification,
            Some(DeliveryMode::Indication) => ObservedDelivery::Indication,
            None => ObservedDelivery::Unknown,
        });
        Ok(observed)
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
                }
            }
        }
    }

    fn take_close_failures(&self) -> Vec<RadioCloseFailure> {
        std::mem::take(&mut self.state.lock().expect("fake radio state").close_failures)
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

    async fn write_without_response_ready(&self, peer_id: &str) -> Result<bool, DesktopError> {
        self.record("write_without_response_ready");
        let scripted = self
            .state
            .lock()
            .expect("fake radio state")
            .write_readiness
            .get(peer_id)
            .copied();
        scripted.ok_or_else(|| unsupported("gatt.write-readiness", "no readiness scripted"))
    }

    async fn characteristic_access(
        &self,
        peer_id: &str,
    ) -> Result<HashMap<InstanceKey, CharacteristicAccess>, DesktopError> {
        self.record("characteristic_access");
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
        radio.push_event(RadioEvent::Connected("peer-1".to_owned()));
        radio.push_event(RadioEvent::Disconnected("peer-1".to_owned()));
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
