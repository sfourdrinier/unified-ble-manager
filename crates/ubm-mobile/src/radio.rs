//! The platform radio seam of the mobile Rust owner.
//!
//! Rust asks, the platform answers. [`PlatformRadio::submit`] hands one
//! typed [`RadioRequest`] to the native adapter (Kotlin over
//! `OwnedAndroidGattRadio`, Swift over `OwnedCoreBluetoothProtocolRadio`);
//! the adapter answers exactly once with [`crate::MobileHost::complete`]
//! and a [`RadioCompletion`] carrying the same request id. Unsolicited
//! platform facts (advertisements, link changes, notifications, adapter
//! state, security changes, restoration) arrive through
//! [`crate::MobileHost::ingest`] as [`RadioIngress`].
//!
//! Every type here is plain data: the JNI and UniFFI facades translate it
//! field by field, so the platform side never parses JSON and never holds
//! a second copy of the schema.

use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState};
pub use ubm_desktop::ReadProvenance;
use ubm_desktop::{
    DeliveryMode, DesktopError, ObservedDelivery, PlatformDetail, PlatformValue, ServiceSnapshot,
};

/// Rust-minted id of one radio request. Unique per host, never reused.
pub type RequestId = u64;

/// The native radio adapter. Implementations must not block: `submit`
/// enqueues the work on the adapter's own thread/queue and returns.
pub trait PlatformRadio: Send + Sync + 'static {
    /// Start one request. The adapter answers it exactly once through
    /// [`crate::MobileHost::complete`], possibly before `submit` returns.
    fn submit(&self, request: RadioRequest);
    /// Best-effort cancel of an in-flight request. The adapter may still
    /// answer it; Rust counts that answer as a late completion.
    fn cancel(&self, request_id: RequestId);
}

/// Wakes the JavaScript side of one session: "records are waiting, call
/// `drain`". Called at most once per armed period, never while a drain is
/// pending. Must not block.
pub trait WakeSink: Send + Sync + 'static {
    fn wake(&self, session_id: u64);
}

/// Which native radio the host drives. Decides the platform rules the
/// legacy route applied (delivery-mode checks, connect intents, scan
/// platform options).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MobilePlatform {
    Android,
    Apple,
}

impl MobilePlatform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Apple => "apple",
        }
    }
}

/// One characteristic instance address. Occurrences select among
/// duplicate UUIDs; the UUID alone never identifies an instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Instance {
    pub peer_id: String,
    pub service_uuid: String,
    pub service_occurrence: u64,
    pub characteristic_uuid: String,
    pub characteristic_occurrence: u64,
}

/// One descriptor address under a characteristic instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DescriptorAddress {
    pub instance: Instance,
    pub descriptor_uuid: String,
    pub descriptor_occurrence: u64,
}

/// Android `ScanSettings` scan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanMode {
    LowPower,
    Balanced,
    LowLatency,
    Opportunistic,
}

impl ScanMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowPower => "low-power",
            Self::Balanced => "balanced",
            Self::LowLatency => "low-latency",
            Self::Opportunistic => "opportunistic",
        }
    }
}

/// Android `ScanSettings` callback type. `match-lost` is not offered: the
/// radio has no loss representation (legacy rejected it the same way).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanCallbackType {
    AllMatches,
    FirstMatch,
}

impl ScanCallbackType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllMatches => "all-matches",
            Self::FirstMatch => "first-match",
        }
    }
}

/// Android-only scan settings (legacy `scanOptions` fields 3–5). `None`
/// fields keep the platform default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AndroidScanOptions {
    pub mode: Option<ScanMode>,
    pub callback_type: Option<ScanCallbackType>,
    pub legacy: Option<bool>,
}

/// One physical scan. Duplicates are always reported (legacy
/// `allowDuplicates = true`); duplicate policy is applied above the radio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanRequest {
    /// Canonical 128-bit service UUIDs; empty = broad scan.
    pub service_uuids: Vec<String>,
    /// Canonical device addresses (Android `ScanFilter.setDeviceAddress`);
    /// empty = any device.
    pub device_addresses: Vec<String>,
    /// Android scan settings; always `None` on Apple.
    pub android: Option<AndroidScanOptions>,
}

/// Android `BluetoothGatt.requestConnectionPriority` levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionPriority {
    LowPower,
    Balanced,
    HighThroughput,
}

impl ConnectionPriority {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LowPower => "low-power",
            Self::Balanced => "balanced",
            Self::HighThroughput => "high-throughput",
        }
    }
}

/// LE PHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phy {
    Le1m,
    Le2m,
    LeCoded,
}

impl Phy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Le1m => "le-1m",
            Self::Le2m => "le-2m",
            Self::LeCoded => "le-coded",
        }
    }
}

/// PHYs the controller reports for one link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhyObservation {
    pub tx: Phy,
    pub rx: Phy,
}

/// Bond transport for `CreateBond` (`auto` = platform default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PairTransport {
    Auto,
    Le,
}

impl PairTransport {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Le => "le",
        }
    }
}

/// What a background lease keeps alive (legacy `acquireBackground` kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundKind {
    /// Android foreground service of type `connectedDevice`.
    ConnectedDevice,
}

impl BackgroundKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectedDevice => "connected-device",
        }
    }
}

/// Closed string vocabularies used by [`SecurityState`] and
/// [`AdapterSnapshot`]; each value is the frozen wire string.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }
        impl $name {
            /// Frozen wire string.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            /// Parse the frozen wire string; anything else is `None`.
            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
            /// Every member, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

wire_enum!(
    /// Bond state as the platform reports it.
    BondState { Bonded => "bonded", Bonding => "bonding", NotBonded => "not-bonded", Unknown => "unknown", Unsupported => "unsupported" }
);
wire_enum!(
    /// Link encryption as the platform reports it.
    EncryptionState { Encrypted => "encrypted", NotEncrypted => "not-encrypted", Unknown => "unknown", Unsupported => "unsupported" }
);
wire_enum!(
    /// Link authentication as the platform reports it.
    AuthenticationState { Authenticated => "authenticated", Unauthenticated => "unauthenticated", Unknown => "unknown", Unsupported => "unsupported" }
);
wire_enum!(
    /// LE Secure Connections as the platform reports it.
    SecureConnectionsState { Yes => "yes", No => "no", Unknown => "unknown", Unsupported => "unsupported" }
);
wire_enum!(
    /// Adapter availability (`AdapterAvailability`).
    AdapterAvailability { Available => "available", Unavailable => "unavailable", Unsupported => "unsupported", Unknown => "unknown" }
);
wire_enum!(
    /// Bluetooth authorization (`AdapterAuthorization`).
    AdapterAuthorization { Granted => "granted", Denied => "denied", Restricted => "restricted", NotDetermined => "not-determined", Unavailable => "unavailable", Unknown => "unknown" }
);
wire_enum!(
    /// Adapter power (`AdapterPower`).
    AdapterPower { On => "on", Off => "off", Resetting => "resetting", Unsupported => "unsupported", Unknown => "unknown" }
);

/// Per-peer security facts (legacy `AndroidSecurityState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityState {
    pub bond: BondState,
    pub encryption: EncryptionState,
    pub authentication: AuthenticationState,
    pub secure_connections: SecureConnectionsState,
    /// Whether a pairing attempt is possible; `None` when unknown.
    pub pairing_possible: Option<bool>,
}

/// Adapter facts as the platform reports them. Authorization, resetting
/// and unsupported states are all first-class (the desktop power enum
/// cannot carry them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSnapshot {
    pub availability: AdapterAvailability,
    pub authorization: AdapterAuthorization,
    pub power: AdapterPower,
    /// Short user-safe reason when the adapter is not usable.
    pub safe_reason: Option<String>,
}

/// One bonded peer from the system bond table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BondedPeer {
    pub peer_id: String,
    pub name: Option<String>,
}

/// One peer the OS handed back through state restoration
/// (`willRestoreState` on Apple; FGS/companion re-attach on Android).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredPeer {
    pub peer_id: String,
    pub name: Option<String>,
    /// Whether the OS still holds the link. A connected restored peer is
    /// adopted with `connection.connect`, which the adapter answers from
    /// the live link without a new connect.
    pub connected: bool,
}

/// One characteristic scope whose close-time release failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFailure {
    pub instance: Instance,
    pub detail: String,
}

/// Closed set of platform failure kinds. The mapping to a contract error
/// identity happens once, in Rust ([`PlatformFailure::to_error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// The link is not connected.
    NotConnected,
    /// The platform does not know this peer id.
    PeerUnknown,
    /// The addressed service/characteristic/descriptor is gone.
    PathStale,
    /// The platform refused because another operation holds the resource.
    Busy,
    /// Bluetooth permission/authorization denied.
    PermissionDenied,
    /// Bluetooth use is restricted (parental controls, MDM).
    PermissionRestricted,
    /// The user has not answered the Bluetooth permission prompt yet.
    PermissionNotDetermined,
    /// The adapter is off.
    AdapterOff,
    /// No usable adapter (absent, unsupported, or the stack is down).
    AdapterUnavailable,
    /// The adapter is resetting.
    AdapterResetting,
    /// The stack answered with a GATT status (see `gatt_status`).
    GattStatus,
    /// The request was cancelled on the platform side.
    Cancelled,
    /// The platform cannot perform this request at all.
    Unsupported,
    /// Any other platform failure.
    Platform,
}

impl FailureKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotConnected => "not-connected",
            Self::PeerUnknown => "peer-unknown",
            Self::PathStale => "path-stale",
            Self::Busy => "busy",
            Self::PermissionDenied => "permission-denied",
            Self::PermissionRestricted => "permission-restricted",
            Self::PermissionNotDetermined => "permission-not-determined",
            Self::AdapterOff => "adapter-off",
            Self::AdapterUnavailable => "adapter-unavailable",
            Self::AdapterResetting => "adapter-resetting",
            Self::GattStatus => "gatt-status",
            Self::Cancelled => "cancelled",
            Self::Unsupported => "unsupported",
            Self::Platform => "platform",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == text)
    }

    pub const ALL: &'static [Self] = &[
        Self::NotConnected,
        Self::PeerUnknown,
        Self::PathStale,
        Self::Busy,
        Self::PermissionDenied,
        Self::PermissionRestricted,
        Self::PermissionNotDetermined,
        Self::AdapterOff,
        Self::AdapterUnavailable,
        Self::AdapterResetting,
        Self::GattStatus,
        Self::Cancelled,
        Self::Unsupported,
        Self::Platform,
    ];
}

/// A platform's answer that the request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformFailure {
    pub kind: FailureKind,
    /// Android `BluetoothGatt` status / CoreBluetooth `CBATTError` code.
    pub gatt_status: Option<i32>,
    /// Apple: the `NSError` domain and code the platform failed with
    /// (`CBATTErrorDomain`, `CBErrorDomain`, the owned radio's domain).
    /// Android reports none: its identity is the GATT status.
    pub native_domain: Option<String>,
    pub native_code: Option<i64>,
    /// Android: the platform's own named failure code when it has one (a
    /// foreground-service or companion-chooser code such as
    /// `foregroundServiceNotConfigured`), which legacy's Expo layer mapped
    /// (finding 133).
    pub native_name: Option<String>,
    /// Platform detail text (never part of the error identity).
    pub detail: String,
    /// `false` when the platform refused the request before sending
    /// anything to the peer (a full write-without-response queue, an
    /// oversize value): a failed write then reports commit
    /// `not-dispatched`. `true` (the default) means the request may have
    /// reached the peer.
    pub dispatched: bool,
}

impl PlatformFailure {
    #[must_use]
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            gatt_status: None,
            native_domain: None,
            native_code: None,
            native_name: None,
            detail: detail.into(),
            dispatched: true,
        }
    }

    /// A failure the platform answered before sending anything.
    #[must_use]
    pub fn not_dispatched(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            dispatched: false,
            ..Self::new(kind, detail)
        }
    }

    /// The contract error this failure reports for one request kind, with
    /// legacy React Native's error identity (finding 113,
    /// `rn-android-boundary.ts` `nativeOperationFailure` on 4.x): a radio
    /// failure is `platform.failure` carrying the platform's own domain,
    /// code and status, except a link loss (`connection.lost` on every
    /// platform, with the platform's answer kept), a cancel, a permission or
    /// adapter state, and CoreBluetooth's read/notify refusals, which keep
    /// their contract codes.
    ///
    /// A link loss is one word on every platform (owner decision, 5.0,
    /// superseding finding 132's Apple rule, which kept `platform.failure`):
    /// `not-connected` on Android and Apple, and Android GATT status 19.
    #[must_use]
    pub fn to_error(&self, kind: RequestKind, platform: MobilePlatform) -> DesktopError {
        let operation = kind.operation();
        let android = platform == MobilePlatform::Android;
        let link_lost = self.kind == FailureKind::NotConnected
            || (android && self.gatt_status == Some(ANDROID_GATT_CONN_TIMEOUT_STATUS));
        let (code, domain, detailed) = match self.kind {
            _ if link_lost => (
                BleErrorCode::ConnectionLost,
                BleErrorDomain::Connection,
                true,
            ),
            FailureKind::PeerUnknown => (
                BleErrorCode::PeerNotFound,
                BleErrorDomain::Connection,
                false,
            ),
            FailureKind::PathStale => (BleErrorCode::GattStaleHandle, BleErrorDomain::Gatt, false),
            FailureKind::PermissionDenied => (
                BleErrorCode::PermissionDenied,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::PermissionRestricted => (
                BleErrorCode::PermissionRestricted,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::PermissionNotDetermined => (
                BleErrorCode::PermissionNotDetermined,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::AdapterOff => (
                BleErrorCode::AdapterPoweredOff,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::AdapterUnavailable => (
                BleErrorCode::AdapterUnavailable,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::AdapterResetting => (
                BleErrorCode::AdapterResetting,
                BleErrorDomain::Adapter,
                false,
            ),
            FailureKind::Cancelled => (BleErrorCode::OperationAborted, BleErrorDomain::Core, false),
            FailureKind::Unsupported => (
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                false,
            ),
            FailureKind::NotConnected
            | FailureKind::Busy
            | FailureKind::GattStatus
            | FailureKind::Platform => {
                if kind == RequestKind::AdapterState {
                    (
                        BleErrorCode::AdapterUnavailable,
                        BleErrorDomain::Adapter,
                        true,
                    )
                } else {
                    (
                        BleErrorCode::PlatformFailure,
                        BleErrorDomain::Platform,
                        true,
                    )
                }
            }
        };
        let mut detail = format!("{}: {}", self.kind.as_str(), self.detail);
        if let Some(status) = self.gatt_status {
            detail = format!("{detail} (gatt-status={status})");
        }
        let mut error = DesktopError::new(code, domain, operation).with_detail(detail);
        if detailed || self.native_name.is_some() {
            error = error.with_platform(self.platform_detail(kind, platform, link_lost));
        }
        if self.dispatched {
            // Finding 183: a submitted GATT operation the Android stack
            // answers with `GATT_INTERNAL_ERROR` (129) — typically the first
            // request after a reconnect — is the stack's own transient
            // glitch, not a link event: the link survives and a retry
            // passes. Like finding 149's 133/HCI 0x3E, the identity stays
            // `platform.failure` and repeating it is the caller's policy
            // (`caller-decides`); the library never retries it itself.
            if android
                && self.kind == FailureKind::GattStatus
                && self.gatt_status == Some(ANDROID_GATT_INTERNAL_ERROR_STATUS)
            {
                error.with_outcome(None, ubm_desktop::Retryability::CallerDecides)
            } else {
                error
            }
        } else {
            error.with_outcome(
                Some(CommitState::NotDispatched),
                ubm_desktop::Retryability::CallerDecides,
            )
        }
    }

    /// Legacy's platform detail: Android `{domain:"android", code:<native
    /// code>, metadata:{androidGattStatus}}`; Apple the `NSError` domain and
    /// code, else `{domain:"corebluetooth", code:<native code>}`.
    fn platform_detail(
        &self,
        kind: RequestKind,
        platform: MobilePlatform,
        link_lost: bool,
    ) -> PlatformDetail {
        let detail = match platform {
            MobilePlatform::Android => {
                let code = match &self.native_name {
                    Some(name) => name.as_str(),
                    None if link_lost => "connectionLost",
                    None => kind.android_native_code(),
                };
                let detail = PlatformDetail::new("android", code);
                match self.gatt_status {
                    Some(status) => detail
                        .with_metadata("androidGattStatus", PlatformValue::Int(i64::from(status))),
                    None => detail,
                }
            }
            MobilePlatform::Apple => PlatformDetail::new(
                self.native_domain.as_deref().unwrap_or("corebluetooth"),
                self.native_code.map_or_else(
                    || kind.apple_native_code().to_owned(),
                    |code| code.to_string(),
                ),
            ),
        };
        detail.with_message(self.detail.clone())
    }
}

/// Android `GATT_CONN_TIMEOUT` (0x13): legacy reported it as a link loss.
const ANDROID_GATT_CONN_TIMEOUT_STATUS: i32 = 19;
/// Android `GATT_INTERNAL_ERROR` (0x81): the stack's transient answer to a
/// submitted request, typically right after a reconnect (finding 183).
const ANDROID_GATT_INTERNAL_ERROR_STATUS: i32 = 129;

/// Which verb a request is; selects the expected completion shape and the
/// failure identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    AdapterState,
    StartScan,
    StopScan,
    Connect,
    Disconnect,
    Discover,
    Read,
    Write,
    ReadDescriptor,
    WriteDescriptor,
    EnableNotifications,
    DisableNotifications,
    ReadMtu,
    ReadWriteLimits,
    RequestMtu,
    ReadRssi,
    RequestConnectionPriority,
    ReadPhy,
    RequestPhy,
    SecurityState,
    CreateBond,
    CancelBond,
    BondedPeers,
    AcquireBackground,
    ReleaseBackground,
    UpdateBackgroundNotification,
    AssociateCompanion,
    ListCompanion,
    DisassociateCompanion,
    ObservePresence,
    StopPresence,
    Close,
}

impl RequestKind {
    /// Contract operation path for errors of this verb.
    #[must_use]
    pub const fn operation(self) -> &'static str {
        match self {
            Self::AdapterState => "adapter.state",
            Self::StartScan => "scan.start",
            Self::StopScan => "scan.stop",
            Self::Connect => "connection.connect",
            Self::Disconnect => "connection.disconnect",
            Self::Discover => "gatt.discover",
            Self::Read => "gatt.read",
            Self::Write => "gatt.write",
            Self::ReadDescriptor => "gatt.read-descriptor",
            Self::WriteDescriptor => "gatt.write-descriptor",
            Self::EnableNotifications => "gatt.subscribe",
            Self::DisableNotifications => "gatt.unsubscribe",
            Self::ReadMtu => "connection.effective-mtu",
            Self::ReadWriteLimits => "gatt.write-limits",
            Self::RequestMtu => "connection.request-mtu",
            Self::ReadRssi => "connection.rssi",
            Self::RequestConnectionPriority => "connection.request-priority",
            Self::ReadPhy => "connection.read-phy",
            Self::RequestPhy => "connection.request-phy",
            Self::SecurityState => "security.state",
            Self::CreateBond => "security.pair",
            Self::CancelBond => "security.cancel-pairing",
            Self::BondedPeers => "peers.bonded",
            Self::AcquireBackground => "background.acquire",
            Self::ReleaseBackground => "background.release",
            Self::UpdateBackgroundNotification => "background.update-notification",
            Self::AssociateCompanion => "companion.associate",
            Self::ListCompanion => "companion.list",
            Self::DisassociateCompanion => "companion.disassociate",
            Self::ObservePresence => "presence.observe",
            Self::StopPresence => "presence.unobserve",
            Self::Close => "radio.close",
        }
    }

    /// Legacy Android dispatcher's failure code for this verb (4.x
    /// `UnifiedBleProtocolAndroidDispatcher.kt`).
    #[must_use]
    pub const fn android_native_code(self) -> &'static str {
        match self {
            Self::StartScan => "scanFailed",
            Self::StopScan => "scanStopFailed",
            Self::Connect => "connectionFailed",
            Self::Disconnect => "disconnectCleanupFailed",
            Self::Discover => "discoverFailed",
            Self::Read => "readFailed",
            Self::Write => "writeFailed",
            Self::ReadDescriptor => "readDescriptorFailed",
            Self::WriteDescriptor => "writeDescriptorFailed",
            Self::EnableNotifications | Self::DisableNotifications => "subscriptionFailed",
            Self::ReadMtu | Self::ReadWriteLimits => "readMtuFailed",
            Self::RequestMtu => "requestMtuFailed",
            Self::ReadRssi => "readRssiFailed",
            Self::RequestConnectionPriority => "requestPriorityFailed",
            Self::ReadPhy => "readPhyFailed",
            Self::RequestPhy => "requestPhyFailed",
            Self::CreateBond => "pairRejected",
            Self::Close => "destroyFailed",
            Self::AdapterState
            | Self::SecurityState
            | Self::CancelBond
            | Self::BondedPeers
            | Self::AcquireBackground
            | Self::ReleaseBackground
            | Self::UpdateBackgroundNotification
            | Self::AssociateCompanion
            | Self::ListCompanion
            | Self::DisassociateCompanion
            | Self::ObservePresence
            | Self::StopPresence => "platformFailure",
        }
    }

    /// Legacy Apple execution's failure code for this verb when CoreBluetooth
    /// gave no `NSError` (4.x `UnifiedBleProtocolAppleExecution.mm`).
    #[must_use]
    pub const fn apple_native_code(self) -> &'static str {
        match self {
            Self::StartScan => "scanStartFailed",
            Self::StopScan => "scanStopFailed",
            Self::Connect => "connectFailed",
            Self::Disconnect => "disconnectFailed",
            Self::Discover => "discoverFailed",
            Self::Read => "readFailed",
            Self::Write => "writeFailed",
            Self::ReadDescriptor => "readDescriptorFailed",
            Self::WriteDescriptor => "writeDescriptorFailed",
            Self::EnableNotifications | Self::DisableNotifications => "subscriptionFailed",
            Self::ReadRssi => "readRssiFailed",
            Self::Close => "destroyFailed",
            Self::ReadMtu
            | Self::ReadWriteLimits
            | Self::RequestMtu
            | Self::RequestConnectionPriority
            | Self::ReadPhy
            | Self::RequestPhy
            | Self::AdapterState
            | Self::SecurityState
            | Self::CreateBond
            | Self::CancelBond
            | Self::BondedPeers
            | Self::AcquireBackground
            | Self::ReleaseBackground
            | Self::UpdateBackgroundNotification
            | Self::AssociateCompanion
            | Self::ListCompanion
            | Self::DisassociateCompanion
            | Self::ObservePresence
            | Self::StopPresence => "platformFailure",
        }
    }

    /// Frozen wire name (JNI `RadioHost` method suffix / UniFFI variant).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AdapterState => "adapter-state",
            Self::StartScan => "start-scan",
            Self::StopScan => "stop-scan",
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Discover => "discover",
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadDescriptor => "read-descriptor",
            Self::WriteDescriptor => "write-descriptor",
            Self::EnableNotifications => "enable-notifications",
            Self::DisableNotifications => "disable-notifications",
            Self::ReadMtu => "read-mtu",
            Self::ReadWriteLimits => "read-write-limits",
            Self::RequestMtu => "request-mtu",
            Self::ReadRssi => "read-rssi",
            Self::RequestConnectionPriority => "request-connection-priority",
            Self::ReadPhy => "read-phy",
            Self::RequestPhy => "request-phy",
            Self::SecurityState => "security-state",
            Self::CreateBond => "create-bond",
            Self::CancelBond => "cancel-bond",
            Self::BondedPeers => "bonded-peers",
            Self::AcquireBackground => "acquire-background",
            Self::ReleaseBackground => "release-background",
            Self::UpdateBackgroundNotification => "update-background-notification",
            Self::AssociateCompanion => "associate-companion",
            Self::ListCompanion => "list-companion",
            Self::DisassociateCompanion => "disassociate-companion",
            Self::ObservePresence => "observe-presence",
            Self::StopPresence => "unobserve-presence",
            Self::Close => "close",
        }
    }
}

/// One request Rust hands to the platform radio. Every variant carries the
/// request id the completion must echo. The expected completion for each
/// variant is documented on it; any other completion is a protocol
/// violation (`protocol.malformed`), never coerced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioRequest {
    /// Current adapter facts. → [`RadioCompletion::Adapter`].
    AdapterState { id: RequestId },
    /// Start the one physical scan. → [`RadioCompletion::Unit`].
    StartScan { id: RequestId, scan: ScanRequest },
    /// Stop the physical scan. → [`RadioCompletion::Unit`].
    StopScan { id: RequestId },
    /// Connect (`auto_connect` = Android `autoConnect`, the legacy
    /// `when-available` intent; always false on Apple). A peer the OS
    /// already holds connected (restored) completes at once.
    /// `preferred_phy` (Android only, never with `auto_connect`) is the set
    /// of LE PHYs the link must be established on — Android's
    /// `connectGatt(…, phy)` mask; empty means no preference. A radio that
    /// cannot establish the link with it (API < 26, or the OS already holds
    /// the link) answers `Failed{Unsupported, dispatched:false}`; it never
    /// connects without it. → [`RadioCompletion::Unit`] once the link is up.
    Connect {
        id: RequestId,
        peer_id: String,
        auto_connect: bool,
        preferred_phy: Vec<Phy>,
    },
    /// Release the link. → [`RadioCompletion::Unit`] once the OS confirms.
    Disconnect { id: RequestId, peer_id: String },
    /// Full service discovery with occurrence indices.
    /// → [`RadioCompletion::Discovered`].
    Discover { id: RequestId, peer_id: String },
    /// → [`RadioCompletion::Read`].
    Read { id: RequestId, instance: Instance },
    /// → [`RadioCompletion::Unit`] (with-response: after the ATT response;
    /// without-response: after the stack accepted the packet).
    Write {
        id: RequestId,
        instance: Instance,
        value: Vec<u8>,
        with_response: bool,
    },
    /// → [`RadioCompletion::Bytes`].
    ReadDescriptor {
        id: RequestId,
        descriptor: DescriptorAddress,
    },
    /// → [`RadioCompletion::Unit`].
    WriteDescriptor {
        id: RequestId,
        descriptor: DescriptorAddress,
        value: Vec<u8>,
    },
    /// Enable (write the CCCD). `requested` is a hard delivery mode the
    /// radio must write (Android) or refuse with `Unsupported` before any
    /// effect. Without one, `preferred` is the mode to write when the
    /// characteristic allows it, else the other one; `None` for both means
    /// notification when allowed, else indication (legacy default). Every
    /// value for this instance must be ingested with this `epoch`.
    /// → [`RadioCompletion::NotifyEnabled`] with the mode actually written
    /// (`Unknown` on Apple).
    EnableNotifications {
        id: RequestId,
        instance: Instance,
        epoch: u64,
        requested: Option<DeliveryMode>,
        preferred: Option<DeliveryMode>,
    },
    /// Disable (clear the CCCD). → [`RadioCompletion::Unit`].
    DisableNotifications { id: RequestId, instance: Instance },
    /// OS-reported ATT MTU (Apple: `maximumWriteValueLength(.withResponse)
    /// + 3`). → [`RadioCompletion::Mtu`] (`None` when not measured).
    ReadMtu { id: RequestId, peer_id: String },
    /// The largest single write the OS accepts on the link, per mode.
    /// Android: with-response is the ATT maximum attribute value (512; the
    /// stack performs the long write), without-response is one ATT payload
    /// of the reported MTU, or of the ATT default MTU 23 before any MTU
    /// exchange. Apple: `maximumWriteValueLength(for:)` per type.
    /// → [`RadioCompletion::WriteLimits`].
    ReadWriteLimits { id: RequestId, peer_id: String },
    /// → [`RadioCompletion::Mtu`] with the negotiated MTU.
    RequestMtu {
        id: RequestId,
        peer_id: String,
        mtu: u16,
    },
    /// Connected RSSI. → [`RadioCompletion::Rssi`].
    ReadRssi { id: RequestId, peer_id: String },
    /// → [`RadioCompletion::Accepted`] (dispatch acceptance only).
    RequestConnectionPriority {
        id: RequestId,
        peer_id: String,
        priority: ConnectionPriority,
    },
    /// → [`RadioCompletion::Phy`].
    ReadPhy { id: RequestId, peer_id: String },
    /// At least one of `tx`/`rx` is set. → [`RadioCompletion::PhyRequest`].
    RequestPhy {
        id: RequestId,
        peer_id: String,
        tx: Option<Phy>,
        rx: Option<Phy>,
    },
    /// → [`RadioCompletion::Security`].
    SecurityState { id: RequestId, peer_id: String },
    /// Create a bond. → [`RadioCompletion::Security`] with the resulting
    /// state (not bonded = rejected).
    CreateBond {
        id: RequestId,
        peer_id: String,
        transport: PairTransport,
    },
    /// Cancel an in-progress bond. → [`RadioCompletion::Unit`].
    CancelBond { id: RequestId, peer_id: String },
    /// System bond table. → [`RadioCompletion::BondedPeers`].
    BondedPeers { id: RequestId },
    /// Hold the process in the background for BLE work (Android:
    /// `connectedDevice` foreground service). → [`RadioCompletion::Lease`].
    AcquireBackground {
        id: RequestId,
        kind: BackgroundKind,
        reason: String,
    },
    /// Release one background lease. → [`RadioCompletion::Unit`].
    ReleaseBackground { id: RequestId, lease_id: String },
    /// Update the foreground-service notification of one lease.
    /// → [`RadioCompletion::Unit`].
    UpdateBackgroundNotification {
        id: RequestId,
        lease_id: String,
        title: String,
        body: Option<String>,
    },
    /// Android `CompanionDeviceManager.associate` chooser.
    /// → [`RadioCompletion::Companion`].
    AssociateCompanion {
        id: RequestId,
        name: Option<String>,
        service_uuid: Option<String>,
    },
    /// This app's `CompanionDeviceManager` associations (finding 236: the
    /// record a duplicate check and the cleanup UI read).
    /// → [`RadioCompletion::CompanionList`].
    ListCompanion { id: RequestId },
    /// Android `CompanionDeviceManager.disassociate` for one association id.
    /// → [`RadioCompletion::Unit`].
    DisassociateCompanion { id: RequestId, association_id: i64 },
    /// Arms Companion Device Manager device presence for one associated peer
    /// (Android API 31+; the session refuses it on Apple).
    /// → [`RadioCompletion::Unit`].
    ObservePresence { id: RequestId, peer_id: String },
    /// Disarms device presence for one peer (idle when none is armed).
    /// → [`RadioCompletion::Unit`].
    StopPresence { id: RequestId, peer_id: String },
    /// Teardown: disable every live notification this radio enabled.
    /// → [`RadioCompletion::Closed`] naming every scope that did not
    /// release (empty = all released).
    Close { id: RequestId },
}

impl RadioRequest {
    #[must_use]
    pub const fn id(&self) -> RequestId {
        match self {
            Self::AdapterState { id }
            | Self::StartScan { id, .. }
            | Self::StopScan { id }
            | Self::Connect { id, .. }
            | Self::Disconnect { id, .. }
            | Self::Discover { id, .. }
            | Self::Read { id, .. }
            | Self::Write { id, .. }
            | Self::ReadDescriptor { id, .. }
            | Self::WriteDescriptor { id, .. }
            | Self::EnableNotifications { id, .. }
            | Self::DisableNotifications { id, .. }
            | Self::ReadMtu { id, .. }
            | Self::ReadWriteLimits { id, .. }
            | Self::RequestMtu { id, .. }
            | Self::ReadRssi { id, .. }
            | Self::RequestConnectionPriority { id, .. }
            | Self::ReadPhy { id, .. }
            | Self::RequestPhy { id, .. }
            | Self::SecurityState { id, .. }
            | Self::CreateBond { id, .. }
            | Self::CancelBond { id, .. }
            | Self::BondedPeers { id }
            | Self::AcquireBackground { id, .. }
            | Self::ReleaseBackground { id, .. }
            | Self::UpdateBackgroundNotification { id, .. }
            | Self::AssociateCompanion { id, .. }
            | Self::ListCompanion { id }
            | Self::DisassociateCompanion { id, .. }
            | Self::ObservePresence { id, .. }
            | Self::StopPresence { id, .. }
            | Self::Close { id } => *id,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RequestKind {
        match self {
            Self::AdapterState { .. } => RequestKind::AdapterState,
            Self::StartScan { .. } => RequestKind::StartScan,
            Self::StopScan { .. } => RequestKind::StopScan,
            Self::Connect { .. } => RequestKind::Connect,
            Self::Disconnect { .. } => RequestKind::Disconnect,
            Self::Discover { .. } => RequestKind::Discover,
            Self::Read { .. } => RequestKind::Read,
            Self::Write { .. } => RequestKind::Write,
            Self::ReadDescriptor { .. } => RequestKind::ReadDescriptor,
            Self::WriteDescriptor { .. } => RequestKind::WriteDescriptor,
            Self::EnableNotifications { .. } => RequestKind::EnableNotifications,
            Self::DisableNotifications { .. } => RequestKind::DisableNotifications,
            Self::ReadMtu { .. } => RequestKind::ReadMtu,
            Self::ReadWriteLimits { .. } => RequestKind::ReadWriteLimits,
            Self::RequestMtu { .. } => RequestKind::RequestMtu,
            Self::ReadRssi { .. } => RequestKind::ReadRssi,
            Self::RequestConnectionPriority { .. } => RequestKind::RequestConnectionPriority,
            Self::ReadPhy { .. } => RequestKind::ReadPhy,
            Self::RequestPhy { .. } => RequestKind::RequestPhy,
            Self::SecurityState { .. } => RequestKind::SecurityState,
            Self::CreateBond { .. } => RequestKind::CreateBond,
            Self::CancelBond { .. } => RequestKind::CancelBond,
            Self::BondedPeers { .. } => RequestKind::BondedPeers,
            Self::AcquireBackground { .. } => RequestKind::AcquireBackground,
            Self::ReleaseBackground { .. } => RequestKind::ReleaseBackground,
            Self::UpdateBackgroundNotification { .. } => RequestKind::UpdateBackgroundNotification,
            Self::AssociateCompanion { .. } => RequestKind::AssociateCompanion,
            Self::ListCompanion { .. } => RequestKind::ListCompanion,
            Self::DisassociateCompanion { .. } => RequestKind::DisassociateCompanion,
            Self::ObservePresence { .. } => RequestKind::ObservePresence,
            Self::StopPresence { .. } => RequestKind::StopPresence,
            Self::Close { .. } => RequestKind::Close,
        }
    }
}

/// The platform's one answer to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioCompletion {
    Unit,
    /// A descriptor read's value.
    Bytes(Vec<u8>),
    /// A characteristic read's value and what the platform says it is
    /// (Android `onCharacteristicRead` is always the read response;
    /// CoreBluetooth's fused `didUpdateValueFor` is the read response only
    /// while the characteristic cannot notify).
    Read {
        value: Vec<u8>,
        provenance: ReadProvenance,
    },
    Adapter(AdapterSnapshot),
    Discovered(Vec<ServiceSnapshot>),
    NotifyEnabled(ObservedDelivery),
    Mtu(Option<u16>),
    /// Per-mode single-write limits; both are at least one byte.
    WriteLimits(WriteLimits),
    Rssi(i16),
    Accepted(bool),
    Phy(PhyObservation),
    PhyRequest {
        accepted: bool,
        observation: Option<PhyObservation>,
    },
    Security(SecurityState),
    BondedPeers(Vec<BondedPeer>),
    /// Background lease id.
    Lease(String),
    Companion {
        association_id: i64,
        peer_id: Option<String>,
        display_name: Option<String>,
        /// The platform already held this association: nothing new was
        /// created and the record is the existing association (finding
        /// 236). The session reports `already-associated` instead of
        /// `associated` so the caller can tell what happened.
        already_associated: bool,
    },
    CompanionList(Vec<CompanionRecord>),
    Closed(Vec<CloseFailure>),
    Failed(PlatformFailure),
}

/// One of this app's Companion Device Manager associations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionRecord {
    pub association_id: i64,
    pub peer_id: Option<String>,
    pub display_name: Option<String>,
}

impl RadioCompletion {
    /// Whether this completion is the documented answer shape for `kind`
    /// (a failure always is).
    #[must_use]
    pub fn answers(&self, kind: RequestKind) -> bool {
        use RequestKind as K;
        if let (Self::WriteLimits(limits), K::ReadWriteLimits) = (self, kind) {
            return limits.with_response > 0 && limits.without_response > 0;
        }
        matches!(
            (self, kind),
            (Self::Failed(_), _)
                | (Self::Adapter(_), K::AdapterState)
                | (
                    Self::Unit,
                    K::StartScan
                        | K::StopScan
                        | K::Connect
                        | K::Disconnect
                        | K::Write
                        | K::WriteDescriptor
                        | K::DisableNotifications
                        | K::CancelBond
                        | K::ReleaseBackground
                        | K::UpdateBackgroundNotification
                        | K::DisassociateCompanion
                        | K::ObservePresence
                        | K::StopPresence
                )
                | (Self::Lease(_), K::AcquireBackground)
                | (Self::Companion { .. }, K::AssociateCompanion)
                | (Self::CompanionList(_), K::ListCompanion)
                | (Self::Discovered(_), K::Discover)
                | (Self::Read { .. }, K::Read)
                | (Self::Bytes(_), K::ReadDescriptor)
                | (Self::NotifyEnabled(_), K::EnableNotifications)
                | (Self::Mtu(_), K::ReadMtu)
                | (Self::WriteLimits(_), K::ReadWriteLimits)
                | (Self::Mtu(Some(_)), K::RequestMtu)
                | (Self::Rssi(_), K::ReadRssi)
                | (Self::Accepted(_), K::RequestConnectionPriority)
                | (Self::Phy(_), K::ReadPhy)
                | (Self::PhyRequest { .. }, K::RequestPhy)
                | (Self::Security(_), K::SecurityState | K::CreateBond)
                | (Self::BondedPeers(_), K::BondedPeers)
                | (Self::Closed(_), K::Close)
        )
    }
}

/// Manufacturer-data section of an advertisement (company id split from
/// the payload by the adapter: little-endian first two bytes).
pub use ubm_desktop::{ManufacturerData, ServiceData, WriteLimits};

/// One advertisement as the platform observed it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Advertisement {
    pub peer_id: String,
    /// BLE address when the OS exposes one (Android); `None` on Apple.
    pub address: Option<String>,
    pub local_name: Option<String>,
    pub rssi: Option<i16>,
    pub tx_power_level: Option<i16>,
    pub service_uuids: Vec<String>,
    pub manufacturer_data: Vec<ManufacturerData>,
    pub service_data: Vec<ServiceData>,
    /// Whether the advertisement is connectable; `None` when the platform
    /// does not report it.
    pub connectable: Option<bool>,
    /// Solicited service UUIDs; `None` when absent.
    pub solicited_service_uuids: Option<Vec<String>>,
    /// Overflow-area service UUIDs (CoreBluetooth); `None` when absent.
    pub overflow_service_uuids: Option<Vec<String>>,
    /// GAP Appearance (AD type 0x19, Android `ScanRecord`); `None` when the
    /// advertisement did not carry it or the platform does not report it
    /// (CoreBluetooth never does).
    pub appearance: Option<u16>,
    /// The raw advertising record bytes (Android `ScanRecord.getBytes()`);
    /// `None` when not reported (CoreBluetooth never does).
    pub raw_record: Option<Vec<u8>>,
}

/// Ingress class for bounded queues and drop accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IngressClass {
    Advertisement,
    Notification,
    Control,
}

impl IngressClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Advertisement => "advertisement",
            Self::Notification => "notification",
            Self::Control => "control",
        }
    }
}

/// One unsolicited platform fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioIngress {
    Advertisement(Advertisement),
    /// Link change the OS reported (`status` = platform GATT status).
    Connection {
        peer_id: String,
        connected: bool,
        status: Option<i32>,
    },
    /// The peer's GATT database changed (Android `onServiceChanged`,
    /// CoreBluetooth `didModifyServices`).
    ServicesChanged {
        peer_id: String,
    },
    /// A value for an enabled instance, stamped with the enable's epoch.
    Notification {
        instance: Instance,
        epoch: u64,
        value: Vec<u8>,
    },
    AdapterState(AdapterSnapshot),
    /// The OS stopped the scan on its own (Android `onScanFailed`).
    ScanFailed {
        detail: String,
    },
    SecurityChanged {
        peer_id: String,
        state: SecurityState,
    },
    /// Peers the OS handed back through state restoration.
    Restored {
        peers: Vec<RestoredPeer>,
    },
    /// The adapter could not translate one platform fact (for example a
    /// manufacturer section shorter than its 2-byte company id). Counted
    /// and surfaced, never dropped silently.
    Dropped {
        class: IngressClass,
        detail: String,
    },
}

/// What [`crate::MobileHost::ingest`] did with one fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressStatus {
    Accepted,
    /// A bounded queue was full; the drop is counted and surfaced as an
    /// `ingress-drop` record.
    Dropped(IngressClass),
    /// The host is shut down.
    Closed,
}

impl IngressStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Dropped(IngressClass::Advertisement) => "dropped-advertisement",
            Self::Dropped(IngressClass::Notification) => "dropped-notification",
            Self::Dropped(IngressClass::Control) => "dropped-control",
            Self::Closed => "closed",
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::{FailureKind, MobilePlatform, PlatformFailure, RequestKind};

    #[test]
    fn adapter_and_permission_kinds_keep_their_legacy_identities() {
        for (kind, code, domain) in [
            (
                FailureKind::AdapterUnavailable,
                "adapter.unavailable",
                "adapter",
            ),
            (
                FailureKind::AdapterResetting,
                "adapter.resetting",
                "adapter",
            ),
            (FailureKind::AdapterOff, "adapter.powered-off", "adapter"),
            (
                FailureKind::PermissionDenied,
                "permission.denied",
                "adapter",
            ),
            (
                FailureKind::PermissionRestricted,
                "permission.restricted",
                "adapter",
            ),
            (
                FailureKind::PermissionNotDetermined,
                "permission.not-determined",
                "adapter",
            ),
        ] {
            let error = PlatformFailure::new(kind, "x")
                .to_error(RequestKind::StartScan, MobilePlatform::Android);
            assert_eq!(error.code_str(), code, "{}", kind.as_str());
            assert_eq!(error.domain().as_str(), domain);
            assert_eq!(FailureKind::parse(kind.as_str()), Some(kind));
        }
    }

    /// Finding 183: a submitted CCCD write answered with Android
    /// `GATT_INTERNAL_ERROR` (129) right after a reconnect is the stack's
    /// own transient glitch — discovery and the local registration on the
    /// same `BluetoothGatt` succeeded, the old Gatt was closed at teardown,
    /// and a retry passes. It stays `platform.failure` (the link survived)
    /// but reports `caller-decides` like finding 149's 133/HCI 0x3E, never
    /// a silent `never`.
    #[test]
    fn android_gatt_internal_error_is_a_transient_stack_failure() {
        let failure = PlatformFailure {
            gatt_status: Some(129),
            ..PlatformFailure::new(FailureKind::GattStatus, "cccd-write status=129")
        };
        let error = failure.to_error(RequestKind::EnableNotifications, MobilePlatform::Android);
        assert_eq!(error.code_str(), "platform.failure");
        assert_eq!(
            error.retryability(),
            ubm_desktop::Retryability::CallerDecides
        );
    }

    #[test]
    fn a_failure_the_platform_never_sent_says_so() {
        let error = PlatformFailure::not_dispatched(FailureKind::Busy, "queue full")
            .to_error(RequestKind::Write, MobilePlatform::Apple);
        assert_eq!(
            error.commit(),
            Some(ubm_core::contracts::CommitState::NotDispatched)
        );
        assert_eq!(
            PlatformFailure::new(FailureKind::GattStatus, "x")
                .to_error(RequestKind::Write, MobilePlatform::Android)
                .commit(),
            None,
            "a failure after sending carries no commit fact"
        );
    }
}
