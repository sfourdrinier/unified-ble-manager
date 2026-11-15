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

/// Version of the private native ordered-open owner handshake.
pub const ORDERED_ADMISSION_OWNER_VERSION: u32 = 1;
/// Version of the slot identity and request format used by native adapters.
pub const ORDERED_ADMISSION_SLOT_VERSION: u32 = 2;
/// Native adapter negotiation requiring complete ordered failure-summary records.
pub const ORDERED_FAILURE_SUMMARY_ADMISSION_VERSION: u32 = 3;
/// Owner transport version reserved for the native retirement protocol.
/// Native adapters must not advertise it until all of that protocol is implemented.
pub const ORDERED_OWNER_RETIREMENT_VERSION: u32 = 4;

/// Check the exact canonical success record produced by the host cleanup path.
/// Unknown or malformed records fail closed and must retain the binding.
#[must_use]
pub fn binding_release_succeeded(record: &str) -> bool {
    record == r#"{"failures":[],"state":"released"}"#
}

/// Opaque UUID minted once by the native object that owns admission receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderedAdmissionOwner([u8; 16]);

/// The protocol version and the native owner that explicitly negotiated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedAdmissionNegotiation {
    pub version: u32,
    pub owner: OrderedAdmissionOwner,
}

impl OrderedAdmissionNegotiation {
    pub fn negotiate(version: u32, value: &str) -> Result<Self, &'static str> {
        let owner = match version {
            ORDERED_ADMISSION_OWNER_VERSION => OrderedAdmissionOwner::negotiate(version, value)?,
            ORDERED_ADMISSION_SLOT_VERSION => {
                OrderedAdmissionOwner::negotiate_slot(version, value)?
            }
            ORDERED_OWNER_RETIREMENT_VERSION => OrderedAdmissionOwner::parse_uuid(value)?,
            _ => return Err("unsupported ordered admission version"),
        };
        Ok(Self { version, owner })
    }
}

impl std::fmt::Display for OrderedAdmissionOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if [4, 6, 8, 10].contains(&index) {
                formatter.write_str("-")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl OrderedAdmissionOwner {
    /// Accept only canonical lowercase RFC 4122 version-4 UUIDs for this protocol.
    pub fn negotiate(version: u32, value: &str) -> Result<Self, &'static str> {
        if version != ORDERED_ADMISSION_OWNER_VERSION {
            return Err("unsupported ordered admission owner version");
        }
        Self::parse_uuid(value)
    }

    /// Parse the owner returned by a successful version-2 negotiation.
    pub fn negotiate_v2(value: &str) -> Result<Self, &'static str> {
        Self::parse_uuid(value)
    }

    /// Validate an explicitly negotiated slot protocol version and its UUID.
    pub fn negotiate_slot(version: u32, value: &str) -> Result<Self, &'static str> {
        if version != ORDERED_ADMISSION_SLOT_VERSION {
            return Err("unsupported ordered admission slot version");
        }
        Self::parse_uuid(value)
    }

    fn parse_uuid(value: &str) -> Result<Self, &'static str> {
        let bytes = value.as_bytes();
        if bytes.len() != 36 || [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
            return Err("malformed ordered admission owner");
        }
        let mut id = [0u8; 16];
        let mut digit = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            if [8, 13, 18, 23].contains(&i) {
                continue;
            }
            let nibble = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => return Err("malformed ordered admission owner"),
            };
            if digit % 2 == 0 {
                id[digit / 2] = nibble << 4;
            } else {
                id[digit / 2] |= nibble;
            }
            digit += 1;
        }
        if id[6] >> 4 != 4 || id[8] >> 6 != 2 {
            return Err("malformed ordered admission owner");
        }
        Ok(Self(id))
    }

    #[must_use]
    pub fn bytes(self) -> [u8; 16] {
        self.0
    }

    #[cfg(test)]
    pub(crate) fn from_test_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

/// The native radio adapter. Implementations must not block: `submit`
/// enqueues the work on the adapter's own thread/queue and returns.
pub trait PlatformRadio: Send + Sync + 'static {
    /// Identity of the actual native admission owner, if negotiated.
    fn ordered_admission_owner(&self) -> Option<OrderedAdmissionOwner> {
        None
    }
    /// The version actually negotiated. Legacy implementations advertise v1.
    fn ordered_admission_negotiation(&self) -> Option<OrderedAdmissionNegotiation> {
        self.ordered_admission_owner()
            .map(|owner| OrderedAdmissionNegotiation {
                version: ORDERED_ADMISSION_OWNER_VERSION,
                owner,
            })
    }
    /// Independently negotiated owner retirement transport version.
    fn ordered_owner_retirement_version(&self) -> u32 {
        0
    }
    /// Identity of the native owner that implements the v4-wide retirement
    /// fence. This is deliberately independent from the v2/v3 ordered-open
    /// owner: a host may keep slot receipts under one owner while a broader
    /// native coordinator fences scans, connections and callbacks under
    /// another. A v4 implementation must return this exact negotiated owner.
    fn ordered_owner_retirement_owner(&self) -> Option<OrderedAdmissionOwner> {
        None
    }
    /// Availability of the public subrate request API on this native host.
    /// Entitlements are checked by the OS when the request is submitted.
    fn connection_subrate_available(&self) -> bool {
        false
    }
    /// Start one request. The adapter answers it exactly once through
    /// [`crate::MobileHost::complete`], possibly before `submit` returns.
    fn submit(&self, request: RadioRequest);
    /// Best-effort cancel of an in-flight request. The adapter may still
    /// answer it; Rust counts that answer as a late completion.
    fn cancel(&self, request_id: RequestId);
}

/// Native owner retirement is a private v4 transport. A `Quiescent` receipt
/// is proof only when the exact independently negotiated v4 owner reports it;
/// an unnegotiated host must retain cleanup rather than infer quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerOutstandingClass {
    Attachments,
    Reservations,
    NativeWork,
    Callbacks,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerRetirementRejection {
    WrongOwner,
    ConflictingRetirement,
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerRetirementState {
    FencedPending {
        outstanding_classes: Vec<OwnerOutstandingClass>,
        failures: Vec<PlatformFailure>,
    },
    Quiescent,
    Rejected {
        reason: OwnerRetirementRejection,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRetirementResult {
    pub owner_incarnation: OrderedAdmissionOwner,
    pub retirement_id: OrderedAdmissionOwner,
    pub state: OwnerRetirementState,
}

/// Wakes the JavaScript side of one session: "records are waiting, call
/// `drain`". Called at most once per armed period, never while a drain is
/// pending. Must not block.
pub use ubm_desktop::continuation_outbox::WakeSink;

/// Which native radio the host drives. Decides the platform rules the
/// legacy route applied (delivery-mode checks, connect intents, scan
/// platform options).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MobilePlatform {
    Android,
    Apple,
}

/// Portable intent mapped to Android's public OFF/LOW/BALANCED/HIGH presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubrateMode {
    Default,
    LowLatency,
    LowPower,
    HighThroughput,
}

impl SubrateMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::LowLatency => "low-latency",
            Self::LowPower => "low-power",
            Self::HighThroughput => "high-throughput",
        }
    }
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

pub use ubm_core::central::NotificationProvenance as OrderedProvenance;
pub use ubm_core::ordered_notifications::{
    LossScope as OrderedLossScope, LossUnit as OrderedLossUnit,
    SourceGeneration as OrderedSourceGeneration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMember {
    pub member_id: u64,
    pub instance: Instance,
}

/// The exact original open descriptor. Member ordering and all instance
/// fields participate in equality; no UUID or peer normalization is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedOpenSpec {
    pub connection_generation: String,
    pub database_generation: String,
    pub snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity,
    pub members: Vec<OrderedMember>,
    pub item_capacity: u32,
    pub byte_capacity: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderedSlotIdentity {
    pub owner_incarnation: OrderedAdmissionOwner,
    pub slot: u32,
    pub slot_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedOpenIdentity {
    pub version: u32,
    pub owner_incarnation: OrderedAdmissionOwner,
    pub slot: u32,
    pub slot_generation: u64,
    pub original_request_id: RequestId,
    pub native_group_id: u64,
    pub peer_id: String,
    pub spec: OrderedOpenSpec,
}

impl OrderedOpenIdentity {
    /// Peers are compared byte-for-byte, without case folding or other rewriting.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != ORDERED_ADMISSION_SLOT_VERSION
            || self.slot >= 64
            || self.slot_generation == 0
        {
            return Err("invalid ordered open slot identity");
        }
        if self
            .spec
            .members
            .iter()
            .any(|member| member.instance.peer_id != self.peer_id)
        {
            return Err("ordered open peer mismatch");
        }
        Ok(())
    }

    #[must_use]
    pub fn slot_identity(&self) -> OrderedSlotIdentity {
        OrderedSlotIdentity {
            owner_incarnation: self.owner_incarnation,
            slot: self.slot,
            slot_generation: self.slot_generation,
        }
    }
}

/// Private v2 slot operations, distinct from the v1 receipt protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenRequestV2 {
    Open {
        id: RequestId,
        identity: OrderedOpenIdentity,
    },
    Recover {
        id: RequestId,
        identity: OrderedOpenIdentity,
    },
    Acknowledge {
        id: RequestId,
        identity: OrderedOpenIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenOwnershipV2 {
    NotCreated,
    Created {
        source_generation: OrderedSourceGeneration,
        members: Vec<OrderedRegistration>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenOutcomeV2 {
    Pending,
    Opened {
        source_generation: OrderedSourceGeneration,
        members: Vec<OrderedRegistration>,
    },
    Failed {
        original_failure: PlatformFailure,
        ownership: OrderedOpenOwnershipV2,
    },
    Indeterminate {
        original_failure_or_reason: OrderedOpenIndeterminateReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenIndeterminateReason {
    Failure(PlatformFailure),
    Reason(String),
}

/// Only these outcomes may be retained before acknowledging a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenTerminalV2 {
    Opened {
        source_generation: OrderedSourceGeneration,
        members: Vec<OrderedRegistration>,
    },
    Failed {
        original_failure: PlatformFailure,
        ownership: OrderedOpenOwnershipV2,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedOpenResultV2 {
    pub slot_identity: OrderedSlotIdentity,
    pub outcome: OrderedOpenOutcomeV2,
}

impl OrderedOpenResultV2 {
    /// Check the echoed slot and extract a terminal disposition. Callers must
    /// still validate the native generation and registrations against the
    /// original member descriptor before adopting or acknowledging it.
    #[must_use]
    pub fn terminal_for(&self, identity: &OrderedOpenIdentity) -> Option<OrderedOpenTerminalV2> {
        if identity.validate().is_err() || self.slot_identity != identity.slot_identity() {
            return None;
        }
        match &self.outcome {
            OrderedOpenOutcomeV2::Opened {
                source_generation,
                members,
            } => Some(OrderedOpenTerminalV2::Opened {
                source_generation: source_generation.clone(),
                members: members.clone(),
            }),
            OrderedOpenOutcomeV2::Failed {
                original_failure,
                ownership,
            } => Some(OrderedOpenTerminalV2::Failed {
                original_failure: original_failure.clone(),
                ownership: ownership.clone(),
            }),
            OrderedOpenOutcomeV2::Pending | OrderedOpenOutcomeV2::Indeterminate { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedOpenRetired {
    pub slot_identity: OrderedSlotIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenCompletionV2 {
    Result(OrderedOpenResultV2),
    Retired(OrderedOpenRetired),
}

impl OrderedOpenCompletionV2 {
    /// Validate the operation, attempt id, and echoed slot before adoption.
    #[must_use]
    pub fn answers(&self, request: &OrderedOpenRequestV2) -> bool {
        let (id, identity) = match request {
            OrderedOpenRequestV2::Open { id, identity }
            | OrderedOpenRequestV2::Recover { id, identity }
            | OrderedOpenRequestV2::Acknowledge { id, identity } => (*id, identity),
        };
        if identity.validate().is_err()
            || match request {
                OrderedOpenRequestV2::Open { .. } => id != identity.original_request_id,
                _ => id == identity.original_request_id,
            }
        {
            return false;
        }
        let observed = match self {
            Self::Result(result) => result.slot_identity,
            Self::Retired(retired) => retired.slot_identity,
        };
        if observed != identity.slot_identity() {
            return false;
        }
        matches!(
            (self, request),
            (
                Self::Result(_),
                OrderedOpenRequestV2::Open { .. } | OrderedOpenRequestV2::Recover { .. }
            ) | (Self::Retired(_), OrderedOpenRequestV2::Acknowledge { .. })
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderedRegistration {
    pub member_id: u64,
    pub source_member: u64,
    pub epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrderedActivation {
    pub instance: Instance,
    pub connection_generation: String,
    pub source_generation: OrderedSourceGeneration,
    pub group_id: u64,
    pub member: OrderedRegistration,
    pub activation_epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedMarkerAdmission {
    Pending,
    NotAdmitted,
    Admitted { ordinal: u64 },
    Rejected { failure: PlatformFailure },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedMarkerWriteState {
    Initiating,
    NotStarted { failure: Option<PlatformFailure> },
    Started { failure: Option<PlatformFailure> },
    Settled { failure: Option<PlatformFailure> },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMarkerWriteStatus {
    pub admission: OrderedMarkerAdmission,
    pub write: OrderedMarkerWriteState,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedEnableStatus {
    Pending,
    Enabled(ObservedDelivery),
    Released(Option<PlatformFailure>),
    Uncertain(PlatformFailure),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedOpenOwnership {
    NotCreated,
    Created {
        generation: OrderedSourceGeneration,
        members: Vec<OrderedRegistration>,
    },
    Indeterminate {
        recovery_key: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedRejected {
    Value {
        member: OrderedRegistration,
        byte_count: u64,
    },
    Loss {
        scope: OrderedLossScope,
        unit: OrderedLossUnit,
        count: Option<u64>,
        byte_count: Option<u64>,
    },
    Fence {
        member: OrderedRegistration,
    },
    Barrier {
        barrier_id: u64,
    },
    Marker {
        marker_id: u64,
    },
    Unmapped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedEnd {
    Closed,
    ConnectionLost,
    ServiceChanged,
    SourceReplaced,
    Shutdown,
    AdapterLost,
    Overflow,
    OrdinalExhausted,
    SourceFailed,
    WakeFailed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedRecord {
    Value {
        member: OrderedRegistration,
        ordinal: u64,
        value: Vec<u8>,
        delivery: ObservedDelivery,
        provenance: OrderedProvenance,
    },
    Loss {
        scope: OrderedLossScope,
        unit: OrderedLossUnit,
        count: Option<u64>,
        byte_count: Option<u64>,
    },
    Fence {
        member: OrderedRegistration,
        uncertain_tail: bool,
    },
    Barrier {
        barrier_id: u64,
        uncertain_tail: bool,
    },
    Marker {
        marker_id: u64,
        ordinal: u64,
    },
    Terminal {
        reason: OrderedEnd,
        uncertain_tail: bool,
        native_domain: Option<String>,
        native_code: Option<String>,
        failure_member: Option<OrderedRegistration>,
        rejected: Option<OrderedRejected>,
    },
    /// A source terminal carrying the platform's classified failure.
    SourceFailed {
        uncertain_tail: bool,
        failure: PlatformFailure,
        native_domain: Option<String>,
        native_code: Option<String>,
        failure_member: Option<OrderedRegistration>,
        rejected: Option<OrderedRejected>,
    },
    /// A distinct native failure observed after the source terminal.
    SubsequentSourceFailure {
        failure: PlatformFailure,
    },
    /// Native retention overflowed; the number of omitted failures is unknown.
    FailureObservationsOmitted,
    BridgeFailure {
        code: String,
        detail: String,
    },
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

/// Android scan PHY selection; only nonlegacy scans can select a PHY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanPhy {
    AllSupported,
    Le1m,
    LeCoded,
}

impl ScanPhy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllSupported => "all-supported",
            Self::Le1m => "1m",
            Self::LeCoded => "coded",
        }
    }
}

/// Android-only scan settings. `None`
/// fields keep the platform default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AndroidScanOptions {
    pub mode: Option<ScanMode>,
    pub callback_type: Option<ScanCallbackType>,
    pub legacy: Option<bool>,
    pub report_delay_ms: Option<u32>,
    pub phy: Option<ScanPhy>,
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

/// A current system LE connection, independent of this owner's leases or bonds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedPeer {
    pub peer_id: String,
    pub name: Option<String>,
}

/// Native identifier lookup. Existence grants neither a connection nor ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPeer {
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
        // A classified contract code (for example permission-denied) does
        // not erase the native answer which produced it. Retirement and close
        // receipts can carry several such independent failures, so a field
        // present at the native boundary must remain observable rather than
        // being hidden merely because the Rust classification is more
        // specific than `platform.failure`.
        if detailed
            || self.native_name.is_some()
            || self.native_domain.is_some()
            || self.native_code.is_some()
            || self.gatt_status.is_some()
        {
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

    /// Preserve the source's typed platform cause without claiming a link event.
    pub fn to_notification_source_error(&self, platform: MobilePlatform) -> DesktopError {
        let mapped = self.to_error(RequestKind::EnableNotifications, platform);
        let (code, domain) = if mapped.code() == BleErrorCode::ConnectionLost {
            (BleErrorCode::PlatformFailure, BleErrorDomain::Platform)
        } else {
            (mapped.code(), mapped.domain())
        };
        let mut error = DesktopError::new(code, domain, "gatt.notification.source");
        if let Some(detail) = mapped.detail() {
            error = error.with_detail(detail);
        }
        if let Some(platform) = mapped.platform() {
            error = error.with_platform(platform.clone());
        }
        error
    }

    /// Keep the original source-failure fields alongside the normalized
    /// public error for the acknowledged Rust ingress path. The fallback
    /// domain identifies this library's evidence envelope, not a native
    /// platform error; native identity, when present, remains in metadata.
    pub(crate) fn to_acknowledged_source_error(&self, platform: MobilePlatform) -> DesktopError {
        use ubm_desktop::errors::{PlatformDetail, PlatformValue};

        let error = self.to_notification_source_error(platform);
        let detail = error.platform().cloned().unwrap_or_else(|| {
            PlatformDetail::new("ubm-mobile", "notification-source-failure-evidence")
        });
        let mut detail = detail
            .with_metadata(
                "sourceFailureKind",
                PlatformValue::Text(self.kind.as_str().into()),
            )
            .with_metadata(
                "sourceFailureDetail",
                PlatformValue::Text(self.detail.clone()),
            )
            .with_metadata(
                "sourceFailureDispatched",
                PlatformValue::Bool(self.dispatched),
            );
        if let Some(status) = self.gatt_status {
            detail = detail.with_metadata(
                "sourceFailureGattStatus",
                PlatformValue::Int(i64::from(status)),
            );
        }
        if let Some(domain) = &self.native_domain {
            detail = detail.with_metadata(
                "sourceFailureNativeDomain",
                PlatformValue::Text(domain.clone()),
            );
        }
        if let Some(code) = self.native_code {
            detail = detail.with_metadata("sourceFailureNativeCode", PlatformValue::Int(code));
        }
        if let Some(name) = &self.native_name {
            detail =
                detail.with_metadata("sourceFailureNativeName", PlatformValue::Text(name.clone()));
        }
        error.with_platform(detail)
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
                let detail = if self.native_domain.as_deref()
                    == Some("android.bluetooth.BluetoothStatusCodes")
                {
                    match self.native_code {
                        Some(status) => detail
                            .with_metadata("androidBluetoothStatus", PlatformValue::Int(status))
                            .with_metadata(
                                "nativeDomain",
                                PlatformValue::Text(
                                    "android.bluetooth.BluetoothStatusCodes".into(),
                                ),
                            ),
                        None => detail,
                    }
                } else if self.native_domain.as_deref()
                    == Some("android.bluetooth.HciEncryptionChange")
                {
                    match self.native_code {
                        Some(status) => detail
                            .with_metadata("androidEncryptionStatus", PlatformValue::Int(status))
                            .with_metadata(
                                "nativeDomain",
                                PlatformValue::Text("android.bluetooth.HciEncryptionChange".into()),
                            ),
                        None => detail,
                    }
                } else {
                    detail
                };
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

/// The public Android API returns BluetoothStatusCodes, not ATT/GATT status.
/// SUCCESS acknowledges submission; it does not report negotiated subrate.
#[must_use]
pub fn subrate_status_completion(status: i32) -> RadioCompletion {
    if status == 0 {
        return RadioCompletion::Accepted(true);
    }
    let (kind, name) = match status {
        1 => (FailureKind::AdapterOff, "ERROR_BLUETOOTH_NOT_ENABLED"),
        2 => (
            FailureKind::PermissionRestricted,
            "ERROR_BLUETOOTH_NOT_ALLOWED",
        ),
        3 => (FailureKind::Platform, "ERROR_DEVICE_NOT_BONDED"),
        6 => (
            FailureKind::PermissionDenied,
            "ERROR_MISSING_BLUETOOTH_CONNECT_PERMISSION",
        ),
        11 => (FailureKind::Unsupported, "FEATURE_NOT_SUPPORTED"),
        i32::MAX => (FailureKind::Platform, "ERROR_UNKNOWN"),
        _ => (FailureKind::Platform, "UNRECOGNIZED_BLUETOOTH_STATUS"),
    };
    RadioCompletion::Failed(PlatformFailure {
        kind,
        gatt_status: None,
        native_domain: Some("android.bluetooth.BluetoothStatusCodes".into()),
        native_code: Some(i64::from(status)),
        native_name: Some(name.into()),
        detail: format!("BluetoothGatt.requestSubrateMode returned {name} ({status})"),
        dispatched: false,
    })
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
    OwnerFence,
    OwnerStatus,
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
    OrderedEnableNotifications,
    OrderedDisableNotifications,
    OrderedEnableSettlement,
    OrderedForgetEnableReceipt,
    DisableNotifications,
    OrderedOpen,
    OrderedRecoverOpen,
    OrderedAcknowledgeOpen,
    OrderedOpenV2,
    OrderedRecoverOpenV2,
    OrderedAcknowledgeOpenV2,
    OrderedRegister,
    OrderedRetire,
    OrderedDrain,
    OrderedBarrier,
    OrderedGraphAuthorityReserve,
    OrderedGraphAuthorityReserveStatus,
    OrderedGraphAuthorityReserveFence,
    OrderedGraphAuthorityReserveForget,
    OrderedGraphAuthorityOpen,
    OrderedGraphAuthorityStatus,
    OrderedGraphAuthorityFenceOpen,
    OrderedGraphAuthorityForgetOpen,
    OrderedGraphAuthorityClose,
    OrderedGraphAuthorityForget,
    OrderedGraphPermitReserve,
    OrderedGraphPermitReserveStatus,
    OrderedGraphPermitReserveFence,
    OrderedGraphPermitForgetRequest,
    OrderedGraphPermitAcquire,
    OrderedGraphPermitRelease,
    OrderedGraphPermitCommit,
    OrderedGraphPermitForget,
    OrderedMarkerWrite,
    OrderedMarkerWriteSettlement,
    OrderedCancelMarkerWrite,
    OrderedForgetMarkerWriteReceipt,
    OrderedAcknowledgeBatch,
    OrderedClose,
    OrderedSeal,
    OrderedRetainClaim,
    OrderedConfirmCleanup,
    OrderedRemove,
    OrderedForgetRemovalReceipt,
    ReadMtu,
    ReadWriteLimits,
    ReadWriteReadiness,
    RequestMtu,
    ReadRssi,
    RequestConnectionPriority,
    RequestSubrate,
    ReadPhy,
    RequestPhy,
    SecurityState,
    CreateBond,
    CancelBond,
    BondedPeers,
    ConnectedPeers,
    ResolvePeer,
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
            Self::OwnerFence | Self::OwnerStatus => "gatt.ordered-owner.retirement",
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
            Self::OrderedEnableNotifications => "gatt.ordered.enable",
            Self::OrderedDisableNotifications => "gatt.ordered.disable",
            Self::OrderedEnableSettlement => "gatt.ordered.enable-settlement",
            Self::OrderedForgetEnableReceipt => "gatt.ordered.forget-enable",
            Self::DisableNotifications => "gatt.unsubscribe",
            Self::OrderedOpen
            | Self::OrderedRecoverOpen
            | Self::OrderedAcknowledgeOpen
            | Self::OrderedOpenV2
            | Self::OrderedRecoverOpenV2
            | Self::OrderedAcknowledgeOpenV2
            | Self::OrderedRegister
            | Self::OrderedRetire
            | Self::OrderedDrain
            | Self::OrderedBarrier
            | Self::OrderedGraphAuthorityReserve
            | Self::OrderedGraphAuthorityReserveStatus
            | Self::OrderedGraphAuthorityReserveFence
            | Self::OrderedGraphAuthorityReserveForget
            | Self::OrderedGraphAuthorityOpen
            | Self::OrderedGraphAuthorityStatus
            | Self::OrderedGraphAuthorityFenceOpen
            | Self::OrderedGraphAuthorityForgetOpen
            | Self::OrderedGraphAuthorityClose
            | Self::OrderedGraphAuthorityForget
            | Self::OrderedGraphPermitReserve
            | Self::OrderedGraphPermitReserveStatus
            | Self::OrderedGraphPermitReserveFence
            | Self::OrderedGraphPermitForgetRequest
            | Self::OrderedGraphPermitAcquire
            | Self::OrderedGraphPermitRelease
            | Self::OrderedGraphPermitCommit
            | Self::OrderedGraphPermitForget
            | Self::OrderedMarkerWrite
            | Self::OrderedMarkerWriteSettlement
            | Self::OrderedCancelMarkerWrite
            | Self::OrderedForgetMarkerWriteReceipt
            | Self::OrderedAcknowledgeBatch
            | Self::OrderedClose
            | Self::OrderedSeal
            | Self::OrderedRetainClaim
            | Self::OrderedConfirmCleanup
            | Self::OrderedRemove => "gatt.ordered-source",
            Self::OrderedForgetRemovalReceipt => "gatt.ordered-source.forget-removal",
            Self::ReadMtu => "connection.effective-mtu",
            Self::ReadWriteLimits => "gatt.write-limits",
            Self::ReadWriteReadiness => "gatt.write-readiness",
            Self::RequestMtu => "connection.request-mtu",
            Self::ReadRssi => "connection.rssi",
            Self::RequestConnectionPriority => "connection.request-priority",
            Self::RequestSubrate => "connection.request-subrate",
            Self::ReadPhy => "connection.read-phy",
            Self::RequestPhy => "connection.request-phy",
            Self::SecurityState => "security.state",
            Self::CreateBond => "security.pair",
            Self::CancelBond => "security.cancel-pairing",
            Self::BondedPeers => "peers.bonded",
            Self::ConnectedPeers => "peers.connected",
            Self::ResolvePeer => "peers.resolve",
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
            Self::OwnerFence | Self::OwnerStatus => "platformFailure",
            Self::StartScan => "scanFailed",
            Self::StopScan => "scanStopFailed",
            Self::Connect => "connectionFailed",
            Self::Disconnect => "disconnectCleanupFailed",
            Self::Discover => "discoverFailed",
            Self::Read => "readFailed",
            Self::Write => "writeFailed",
            Self::ReadDescriptor => "readDescriptorFailed",
            Self::WriteDescriptor => "writeDescriptorFailed",
            Self::EnableNotifications
            | Self::OrderedEnableNotifications
            | Self::OrderedDisableNotifications
            | Self::OrderedEnableSettlement
            | Self::OrderedForgetEnableReceipt
            | Self::DisableNotifications
            | Self::OrderedOpen
            | Self::OrderedRecoverOpen
            | Self::OrderedAcknowledgeOpen
            | Self::OrderedOpenV2
            | Self::OrderedRecoverOpenV2
            | Self::OrderedAcknowledgeOpenV2
            | Self::OrderedRegister
            | Self::OrderedRetire
            | Self::OrderedDrain
            | Self::OrderedBarrier
            | Self::OrderedGraphAuthorityReserve
            | Self::OrderedGraphAuthorityReserveStatus
            | Self::OrderedGraphAuthorityReserveFence
            | Self::OrderedGraphAuthorityReserveForget
            | Self::OrderedGraphAuthorityOpen
            | Self::OrderedGraphAuthorityStatus
            | Self::OrderedGraphAuthorityFenceOpen
            | Self::OrderedGraphAuthorityForgetOpen
            | Self::OrderedGraphAuthorityClose
            | Self::OrderedGraphAuthorityForget
            | Self::OrderedGraphPermitReserve
            | Self::OrderedGraphPermitReserveStatus
            | Self::OrderedGraphPermitReserveFence
            | Self::OrderedGraphPermitForgetRequest
            | Self::OrderedGraphPermitAcquire
            | Self::OrderedGraphPermitRelease
            | Self::OrderedGraphPermitCommit
            | Self::OrderedGraphPermitForget
            | Self::OrderedMarkerWrite
            | Self::OrderedMarkerWriteSettlement
            | Self::OrderedCancelMarkerWrite
            | Self::OrderedForgetMarkerWriteReceipt
            | Self::OrderedAcknowledgeBatch
            | Self::OrderedClose
            | Self::OrderedSeal
            | Self::OrderedRetainClaim
            | Self::OrderedConfirmCleanup
            | Self::OrderedRemove => "subscriptionFailed",
            Self::OrderedForgetRemovalReceipt => "subscriptionFailed",
            Self::ReadMtu | Self::ReadWriteLimits | Self::ReadWriteReadiness => "readMtuFailed",
            Self::RequestMtu => "requestMtuFailed",
            Self::ReadRssi => "readRssiFailed",
            Self::RequestConnectionPriority => "requestPriorityFailed",
            Self::RequestSubrate => "requestSubrateFailed",
            Self::ReadPhy => "readPhyFailed",
            Self::RequestPhy => "requestPhyFailed",
            Self::CreateBond => "pairRejected",
            Self::Close => "destroyFailed",
            Self::AdapterState
            | Self::SecurityState
            | Self::CancelBond
            | Self::BondedPeers
            | Self::ConnectedPeers
            | Self::ResolvePeer
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
            Self::OwnerFence | Self::OwnerStatus => "platformFailure",
            Self::StartScan => "scanStartFailed",
            Self::StopScan => "scanStopFailed",
            Self::Connect => "connectFailed",
            Self::Disconnect => "disconnectFailed",
            Self::Discover => "discoverFailed",
            Self::Read => "readFailed",
            Self::Write => "writeFailed",
            Self::ReadDescriptor => "readDescriptorFailed",
            Self::WriteDescriptor => "writeDescriptorFailed",
            Self::EnableNotifications
            | Self::OrderedEnableNotifications
            | Self::OrderedDisableNotifications
            | Self::OrderedEnableSettlement
            | Self::OrderedForgetEnableReceipt
            | Self::DisableNotifications
            | Self::OrderedOpen
            | Self::OrderedRecoverOpen
            | Self::OrderedAcknowledgeOpen
            | Self::OrderedOpenV2
            | Self::OrderedRecoverOpenV2
            | Self::OrderedAcknowledgeOpenV2
            | Self::OrderedRegister
            | Self::OrderedRetire
            | Self::OrderedDrain
            | Self::OrderedBarrier
            | Self::OrderedGraphAuthorityReserve
            | Self::OrderedGraphAuthorityReserveStatus
            | Self::OrderedGraphAuthorityReserveFence
            | Self::OrderedGraphAuthorityReserveForget
            | Self::OrderedGraphAuthorityOpen
            | Self::OrderedGraphAuthorityStatus
            | Self::OrderedGraphAuthorityFenceOpen
            | Self::OrderedGraphAuthorityForgetOpen
            | Self::OrderedGraphAuthorityClose
            | Self::OrderedGraphAuthorityForget
            | Self::OrderedGraphPermitReserve
            | Self::OrderedGraphPermitReserveStatus
            | Self::OrderedGraphPermitReserveFence
            | Self::OrderedGraphPermitForgetRequest
            | Self::OrderedGraphPermitAcquire
            | Self::OrderedGraphPermitRelease
            | Self::OrderedGraphPermitCommit
            | Self::OrderedGraphPermitForget
            | Self::OrderedMarkerWrite
            | Self::OrderedMarkerWriteSettlement
            | Self::OrderedCancelMarkerWrite
            | Self::OrderedForgetMarkerWriteReceipt
            | Self::OrderedAcknowledgeBatch
            | Self::OrderedClose
            | Self::OrderedSeal
            | Self::OrderedRetainClaim
            | Self::OrderedConfirmCleanup
            | Self::OrderedRemove => "subscriptionFailed",
            Self::OrderedForgetRemovalReceipt => "subscriptionFailed",
            Self::ReadRssi => "readRssiFailed",
            Self::Close => "destroyFailed",
            Self::ReadMtu
            | Self::ReadWriteLimits
            | Self::ReadWriteReadiness
            | Self::RequestMtu
            | Self::RequestConnectionPriority
            | Self::RequestSubrate
            | Self::ReadPhy
            | Self::RequestPhy
            | Self::AdapterState
            | Self::SecurityState
            | Self::CreateBond
            | Self::CancelBond
            | Self::BondedPeers
            | Self::ConnectedPeers
            | Self::ResolvePeer
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
            Self::OwnerFence => "owner-fence",
            Self::OwnerStatus => "owner-status",
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
            Self::OrderedEnableNotifications => "ordered-enable-notifications",
            Self::OrderedDisableNotifications => "ordered-disable-notifications",
            Self::OrderedEnableSettlement => "ordered-enable-settlement",
            Self::OrderedForgetEnableReceipt => "ordered-forget-enable-receipt",
            Self::DisableNotifications => "disable-notifications",
            Self::OrderedOpen => "ordered-open",
            Self::OrderedRecoverOpen => "ordered-recover-open",
            Self::OrderedAcknowledgeOpen => "ordered-acknowledge-open",
            Self::OrderedOpenV2 => "ordered-open-v2",
            Self::OrderedRecoverOpenV2 => "ordered-recover-open-v2",
            Self::OrderedAcknowledgeOpenV2 => "ordered-acknowledge-open-v2",
            Self::OrderedRegister => "ordered-register",
            Self::OrderedRetire => "ordered-retire",
            Self::OrderedDrain => "ordered-drain",
            Self::OrderedBarrier => "ordered-barrier",
            Self::OrderedGraphAuthorityForgetOpen => "ordered-graph-authority-forget-open",
            Self::OrderedGraphAuthorityFenceOpen => "ordered-graph-authority-fence-open",
            Self::OrderedGraphAuthorityStatus => "ordered-graph-authority-status",
            Self::OrderedGraphAuthorityReserveForget => "ordered-graph-authority-reserve-forget",
            Self::OrderedGraphAuthorityReserveFence => "ordered-graph-authority-reserve-fence",
            Self::OrderedGraphAuthorityReserveStatus => "ordered-graph-authority-reserve-status",
            Self::OrderedGraphAuthorityReserve => "ordered-graph-authority-reserve",
            Self::OrderedGraphAuthorityOpen => "ordered-graph-authority-open",
            Self::OrderedGraphAuthorityClose => "ordered-graph-authority-close",
            Self::OrderedGraphAuthorityForget => "ordered-graph-authority-forget",
            Self::OrderedGraphPermitReserve => "ordered-graph-permit-reserve",
            Self::OrderedGraphPermitReserveFence => "ordered-graph-permit-reserve-fence",
            Self::OrderedGraphPermitReserveStatus => "ordered-graph-permit-reserve-status",
            Self::OrderedGraphPermitForgetRequest => "ordered-graph-permit-forget-request",
            Self::OrderedGraphPermitAcquire => "ordered-graph-permit-acquire",
            Self::OrderedGraphPermitRelease => "ordered-graph-permit-release",
            Self::OrderedGraphPermitCommit => "ordered-graph-permit-commit",
            Self::OrderedGraphPermitForget => "ordered-graph-permit-forget",
            Self::OrderedMarkerWrite => "ordered-marker-write",
            Self::OrderedMarkerWriteSettlement => "ordered-marker-write-settlement",
            Self::OrderedCancelMarkerWrite => "ordered-cancel-marker-write",
            Self::OrderedForgetMarkerWriteReceipt => "ordered-forget-marker-write-receipt",
            Self::OrderedClose => "ordered-close",
            Self::OrderedSeal => "ordered-seal",
            Self::OrderedRetainClaim => "ordered-retain-claim",
            Self::OrderedConfirmCleanup => "ordered-confirm-cleanup",
            Self::OrderedRemove => "ordered-remove",
            Self::OrderedForgetRemovalReceipt => "ordered-forget-removal-receipt",
            Self::OrderedAcknowledgeBatch => "ordered-acknowledge-batch",
            Self::ReadMtu => "read-mtu",
            Self::ReadWriteLimits => "read-write-limits",
            Self::ReadWriteReadiness => "read-write-readiness",
            Self::RequestMtu => "request-mtu",
            Self::ReadRssi => "read-rssi",
            Self::RequestConnectionPriority => "request-connection-priority",
            Self::RequestSubrate => "request-subrate",
            Self::ReadPhy => "read-phy",
            Self::RequestPhy => "request-phy",
            Self::SecurityState => "security-state",
            Self::CreateBond => "create-bond",
            Self::CancelBond => "cancel-bond",
            Self::BondedPeers => "bonded-peers",
            Self::ConnectedPeers => "connected-peers",
            Self::ResolvePeer => "resolve-peer",
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

/// Version of the exact ordered-open admission acknowledgement operation.
pub const ORDERED_ACKNOWLEDGE_OPEN_VERSION: u32 = 1;

/// One request Rust hands to the platform radio. Every variant carries the
/// request id the completion must echo. The expected completion for each
/// variant is documented on it; any other completion is a protocol
/// violation (`protocol.malformed`), never coerced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioRequest {
    /// Fence a native owner. Retrying uses a new request id and the same retirement id.
    OwnerFence {
        id: RequestId,
        owner_incarnation: OrderedAdmissionOwner,
        retirement_id: OrderedAdmissionOwner,
    },
    /// Read the retained result for an existing retirement.
    OwnerStatus {
        id: RequestId,
        owner_incarnation: OrderedAdmissionOwner,
        retirement_id: OrderedAdmissionOwner,
    },
    /// Current adapter facts. → [`RadioCompletion::Adapter`].
    AdapterState {
        id: RequestId,
    },
    /// Start the one physical scan. → [`RadioCompletion::Unit`].
    /// The request id also identifies the installed callback generation for
    /// [`RadioIngress::AdvertisementScoped`] and [`RadioIngress::ScanFailedScoped`].
    /// Legacy native bridges do not yet echo this identity.
    StartScan {
        id: RequestId,
        scan: ScanRequest,
    },
    /// Stop the physical scan. → [`RadioCompletion::Unit`].
    StopScan {
        id: RequestId,
    },
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
        /// Core's canonical connection generation admitted for this request.
        expected_generation: String,
    },
    /// Release the link. → [`RadioCompletion::Unit`] once the OS confirms.
    Disconnect {
        id: RequestId,
        peer_id: String,
    },
    /// Full service discovery with occurrence indices.
    /// → [`RadioCompletion::Discovered`].
    Discover {
        id: RequestId,
        peer_id: String,
    },
    /// → [`RadioCompletion::Read`].
    Read {
        id: RequestId,
        instance: Instance,
    },
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
    OrderedEnableNotifications {
        id: RequestId,
        activation: OrderedActivation,
        requested: Option<DeliveryMode>,
        preferred: Option<DeliveryMode>,
    },
    OrderedDisableNotifications {
        id: RequestId,
        activation: OrderedActivation,
    },
    OrderedEnableSettlement {
        id: RequestId,
        activation: OrderedActivation,
    },
    OrderedForgetEnableReceipt {
        id: RequestId,
        activation: OrderedActivation,
    },
    /// Disable (clear the CCCD). → [`RadioCompletion::Unit`].
    DisableNotifications {
        id: RequestId,
        instance: Instance,
        epoch: u64,
    },
    OrderedOpen {
        id: RequestId,
        group_id: u64,
        connection_generation: String,
        database_generation: String,
        snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity,
        members: Vec<OrderedMember>,
        item_capacity: u32,
        byte_capacity: u32,
    },
    /// Slot-based open, recovery, or receipt retirement after v2 negotiation.
    OrderedOpenV2(OrderedOpenRequestV2),
    /// Fence or recover the exact original admission; absence is not no-effect proof.
    OrderedRecoverOpen {
        id: RequestId,
        group_id: u64,
        open_request_id: RequestId,
        peer_id: String,
    },
    /// Retire only the exact terminal admission receipt. Unit also permits an
    /// idempotent retry of an already-retired identity; it says nothing about
    /// whether the source was created or removed.
    OrderedAcknowledgeOpen {
        id: RequestId,
        version: u32,
        group_id: u64,
        open_request_id: RequestId,
        peer_id: String,
    },
    OrderedRegister {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        member: OrderedRegistration,
    },
    OrderedRetire {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        member: OrderedRegistration,
    },
    OrderedDrain {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    OrderedBarrier {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        barrier_id: u64,
    },
    OrderedGraphAuthorityReserve {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityReserveStatus {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityReserveFence {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityReserveForget {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityOpen {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityStatus {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityFenceOpen {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityForgetOpen {
        id: RequestId,
        request: u64,
    },
    OrderedGraphAuthorityClose {
        id: RequestId,
        authority_id: u64,
    },
    OrderedGraphAuthorityForget {
        id: RequestId,
        authority_id: u64,
    },
    OrderedGraphPermitReserve {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimPermitRequest,
    },
    OrderedGraphPermitReserveStatus {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimPermitRequest,
    },
    OrderedGraphPermitReserveFence {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimPermitRequest,
    },
    OrderedGraphPermitForgetRequest {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimPermitRequest,
    },
    OrderedGraphPermitAcquire {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimGraphKey,
    },
    OrderedGraphPermitRelease {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimGraphKey,
    },
    OrderedGraphPermitCommit {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimGraphKey,
    },
    OrderedGraphPermitForget {
        id: RequestId,
        key: ubm_desktop::boundary::OrderedReclaimGraphKey,
    },
    OrderedMarkerWrite {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        marker_id: u64,
        connection_generation: String,
        snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity,
        instance: Instance,
        value: Vec<u8>,
        with_response: bool,
    },
    OrderedMarkerWriteSettlement {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedCancelMarkerWrite {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedForgetMarkerWriteReceipt {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        marker_id: u64,
    },
    OrderedAcknowledgeBatch {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        batch_id: u64,
    },
    OrderedClose {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    OrderedSeal {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    OrderedRetainClaim {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        member: OrderedRegistration,
    },
    OrderedConfirmCleanup {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
        member: OrderedRegistration,
    },
    OrderedRemove {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    OrderedForgetRemovalReceipt {
        id: RequestId,
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    /// OS-reported ATT MTU. Apple does not observe one: a CoreBluetooth
    /// write length can include a long write, so the session answers
    /// `None` without issuing this request. → [`RadioCompletion::Mtu`]
    /// (`None` when not measured).
    ReadMtu {
        id: RequestId,
        peer_id: String,
    },
    /// The largest single write the OS accepts on the link, per mode.
    /// Android: with-response is the ATT maximum attribute value (512; the
    /// stack performs the long write), without-response is one ATT payload
    /// of the reported MTU, or of the ATT default MTU 23 before any MTU
    /// exchange. Apple: `maximumWriteValueLength(for:)` per type.
    /// → [`RadioCompletion::WriteLimits`].
    ReadWriteLimits {
        id: RequestId,
        peer_id: String,
    },
    /// Whether the link can take a write without response now
    /// (CoreBluetooth `canSendWriteWithoutResponse`). Android has no
    /// equivalent signal and answers unsupported. Later changes arrive as
    /// [`RadioIngress::WriteReadiness`]. → [`RadioCompletion::Ready`].
    ReadWriteReadiness {
        id: RequestId,
        peer_id: String,
    },
    /// → [`RadioCompletion::Mtu`] with the negotiated MTU.
    RequestMtu {
        id: RequestId,
        peer_id: String,
        mtu: u16,
    },
    /// Connected RSSI. → [`RadioCompletion::Rssi`].
    ReadRssi {
        id: RequestId,
        peer_id: String,
    },
    /// → [`RadioCompletion::Accepted`] (dispatch acceptance only).
    RequestConnectionPriority {
        id: RequestId,
        peer_id: String,
        priority: ConnectionPriority,
    },
    /// Request acceptance only; never an observed subrate factor.
    RequestSubrate {
        id: RequestId,
        peer_id: String,
        mode: SubrateMode,
    },
    /// → [`RadioCompletion::Phy`].
    ReadPhy {
        id: RequestId,
        peer_id: String,
    },
    /// At least one of `tx`/`rx` is set. → [`RadioCompletion::PhyRequest`].
    RequestPhy {
        id: RequestId,
        peer_id: String,
        tx: Option<Phy>,
        rx: Option<Phy>,
    },
    /// → [`RadioCompletion::Security`].
    SecurityState {
        id: RequestId,
        peer_id: String,
    },
    /// Create a bond. → [`RadioCompletion::Security`] with the resulting
    /// state (not bonded = rejected).
    CreateBond {
        id: RequestId,
        peer_id: String,
        transport: PairTransport,
    },
    /// Cancel an in-progress bond. → [`RadioCompletion::Unit`].
    CancelBond {
        id: RequestId,
        peer_id: String,
    },
    /// System bond table. → [`RadioCompletion::BondedPeers`].
    BondedPeers {
        id: RequestId,
    },
    /// Current OS LE inventory. Apple requires services; Android requires none.
    ConnectedPeers {
        id: RequestId,
        services: Vec<String>,
    },
    ResolvePeer {
        id: RequestId,
        peer_id: String,
    },
    /// Hold the process in the background for BLE work (Android:
    /// `connectedDevice` foreground service). → [`RadioCompletion::Lease`].
    AcquireBackground {
        id: RequestId,
        kind: BackgroundKind,
        reason: String,
    },
    /// Release one background lease. → [`RadioCompletion::Unit`].
    ReleaseBackground {
        id: RequestId,
        lease_id: String,
    },
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
        filters_json: Option<String>,
    },
    /// This app's `CompanionDeviceManager` associations (finding 236: the
    /// record a duplicate check and the cleanup UI read).
    /// → [`RadioCompletion::CompanionList`].
    ListCompanion {
        id: RequestId,
    },
    /// Android `CompanionDeviceManager.disassociate` for one association id.
    /// → [`RadioCompletion::Unit`].
    DisassociateCompanion {
        id: RequestId,
        association_id: i64,
    },
    /// Arms Companion Device Manager device presence for one associated peer
    /// (Android API 31+; the session refuses it on Apple).
    /// → [`RadioCompletion::Unit`].
    ObservePresence {
        id: RequestId,
        peer_id: String,
    },
    /// Disarms device presence for one peer (idle when none is armed).
    /// → [`RadioCompletion::Unit`].
    StopPresence {
        id: RequestId,
        peer_id: String,
    },
    /// Teardown: disable every live notification this radio enabled.
    /// → [`RadioCompletion::Closed`] naming every scope that did not
    /// release (empty = all released).
    Close {
        id: RequestId,
    },
}

impl RadioRequest {
    /// Refuse an unnegotiated or wrong-owner control before the native call.
    pub fn validate_owner_retirement(
        &self,
        version: u32,
        owner: OrderedAdmissionOwner,
    ) -> Result<(), &'static str> {
        match self {
            Self::OwnerFence {
                id,
                owner_incarnation,
                ..
            }
            | Self::OwnerStatus {
                id,
                owner_incarnation,
                ..
            } => {
                if version != ORDERED_OWNER_RETIREMENT_VERSION
                    || *owner_incarnation != owner
                    || *id == 0
                {
                    return Err("owner retirement not negotiated for this owner");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    #[must_use]
    pub const fn id(&self) -> RequestId {
        match self {
            Self::OwnerFence { id, .. } | Self::OwnerStatus { id, .. } => *id,
            Self::OrderedOpenV2(
                OrderedOpenRequestV2::Open { id, .. }
                | OrderedOpenRequestV2::Recover { id, .. }
                | OrderedOpenRequestV2::Acknowledge { id, .. },
            ) => *id,
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
            | Self::OrderedEnableNotifications { id, .. }
            | Self::OrderedDisableNotifications { id, .. }
            | Self::OrderedEnableSettlement { id, .. }
            | Self::OrderedForgetEnableReceipt { id, .. }
            | Self::DisableNotifications { id, .. }
            | Self::OrderedOpen { id, .. }
            | Self::OrderedRecoverOpen { id, .. }
            | Self::OrderedAcknowledgeOpen { id, .. }
            | Self::OrderedRegister { id, .. }
            | Self::OrderedRetire { id, .. }
            | Self::OrderedDrain { id, .. }
            | Self::OrderedBarrier { id, .. }
            | Self::OrderedGraphAuthorityReserve { id, .. }
            | Self::OrderedGraphAuthorityReserveStatus { id, .. }
            | Self::OrderedGraphAuthorityReserveFence { id, .. }
            | Self::OrderedGraphAuthorityReserveForget { id, .. }
            | Self::OrderedGraphAuthorityOpen { id, .. }
            | Self::OrderedGraphAuthorityStatus { id, .. }
            | Self::OrderedGraphAuthorityFenceOpen { id, .. }
            | Self::OrderedGraphAuthorityForgetOpen { id, .. }
            | Self::OrderedGraphAuthorityClose { id, .. }
            | Self::OrderedGraphAuthorityForget { id, .. }
            | Self::OrderedGraphPermitReserve { id, .. }
            | Self::OrderedGraphPermitReserveStatus { id, .. }
            | Self::OrderedGraphPermitReserveFence { id, .. }
            | Self::OrderedGraphPermitForgetRequest { id, .. }
            | Self::OrderedGraphPermitAcquire { id, .. }
            | Self::OrderedGraphPermitRelease { id, .. }
            | Self::OrderedGraphPermitCommit { id, .. }
            | Self::OrderedGraphPermitForget { id, .. }
            | Self::OrderedMarkerWrite { id, .. }
            | Self::OrderedMarkerWriteSettlement { id, .. }
            | Self::OrderedCancelMarkerWrite { id, .. }
            | Self::OrderedForgetMarkerWriteReceipt { id, .. }
            | Self::OrderedAcknowledgeBatch { id, .. }
            | Self::OrderedClose { id, .. }
            | Self::OrderedSeal { id, .. }
            | Self::OrderedRetainClaim { id, .. }
            | Self::OrderedConfirmCleanup { id, .. }
            | Self::OrderedRemove { id, .. }
            | Self::OrderedForgetRemovalReceipt { id, .. }
            | Self::ReadMtu { id, .. }
            | Self::ReadWriteLimits { id, .. }
            | Self::ReadWriteReadiness { id, .. }
            | Self::RequestMtu { id, .. }
            | Self::ReadRssi { id, .. }
            | Self::RequestConnectionPriority { id, .. }
            | Self::RequestSubrate { id, .. }
            | Self::ReadPhy { id, .. }
            | Self::RequestPhy { id, .. }
            | Self::SecurityState { id, .. }
            | Self::CreateBond { id, .. }
            | Self::CancelBond { id, .. }
            | Self::BondedPeers { id }
            | Self::ConnectedPeers { id, .. }
            | Self::ResolvePeer { id, .. }
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
            Self::OwnerFence { .. } => RequestKind::OwnerFence,
            Self::OwnerStatus { .. } => RequestKind::OwnerStatus,
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
            Self::OrderedEnableNotifications { .. } => RequestKind::OrderedEnableNotifications,
            Self::OrderedDisableNotifications { .. } => RequestKind::OrderedDisableNotifications,
            Self::OrderedEnableSettlement { .. } => RequestKind::OrderedEnableSettlement,
            Self::OrderedForgetEnableReceipt { .. } => RequestKind::OrderedForgetEnableReceipt,
            Self::DisableNotifications { .. } => RequestKind::DisableNotifications,
            Self::OrderedOpen { .. } => RequestKind::OrderedOpen,
            Self::OrderedRecoverOpen { .. } => RequestKind::OrderedRecoverOpen,
            Self::OrderedAcknowledgeOpen { .. } => RequestKind::OrderedAcknowledgeOpen,
            Self::OrderedOpenV2(OrderedOpenRequestV2::Open { .. }) => RequestKind::OrderedOpenV2,
            Self::OrderedOpenV2(OrderedOpenRequestV2::Recover { .. }) => {
                RequestKind::OrderedRecoverOpenV2
            }
            Self::OrderedOpenV2(OrderedOpenRequestV2::Acknowledge { .. }) => {
                RequestKind::OrderedAcknowledgeOpenV2
            }
            Self::OrderedRegister { .. } => RequestKind::OrderedRegister,
            Self::OrderedRetire { .. } => RequestKind::OrderedRetire,
            Self::OrderedDrain { .. } => RequestKind::OrderedDrain,
            Self::OrderedBarrier { .. } => RequestKind::OrderedBarrier,
            Self::OrderedGraphAuthorityReserve { .. } => RequestKind::OrderedGraphAuthorityReserve,
            Self::OrderedGraphAuthorityReserveStatus { .. } => {
                RequestKind::OrderedGraphAuthorityReserveStatus
            }
            Self::OrderedGraphAuthorityReserveFence { .. } => {
                RequestKind::OrderedGraphAuthorityReserveFence
            }
            Self::OrderedGraphAuthorityReserveForget { .. } => {
                RequestKind::OrderedGraphAuthorityReserveForget
            }
            Self::OrderedGraphAuthorityOpen { .. } => RequestKind::OrderedGraphAuthorityOpen,
            Self::OrderedGraphAuthorityStatus { .. } => RequestKind::OrderedGraphAuthorityStatus,
            Self::OrderedGraphAuthorityFenceOpen { .. } => {
                RequestKind::OrderedGraphAuthorityFenceOpen
            }
            Self::OrderedGraphAuthorityForgetOpen { .. } => {
                RequestKind::OrderedGraphAuthorityForgetOpen
            }
            Self::OrderedGraphAuthorityClose { .. } => RequestKind::OrderedGraphAuthorityClose,
            Self::OrderedGraphAuthorityForget { .. } => RequestKind::OrderedGraphAuthorityForget,
            Self::OrderedGraphPermitReserve { .. } => RequestKind::OrderedGraphPermitReserve,
            Self::OrderedGraphPermitReserveStatus { .. } => {
                RequestKind::OrderedGraphPermitReserveStatus
            }
            Self::OrderedGraphPermitReserveFence { .. } => {
                RequestKind::OrderedGraphPermitReserveFence
            }
            Self::OrderedGraphPermitForgetRequest { .. } => {
                RequestKind::OrderedGraphPermitForgetRequest
            }
            Self::OrderedGraphPermitAcquire { .. } => RequestKind::OrderedGraphPermitAcquire,
            Self::OrderedGraphPermitRelease { .. } => RequestKind::OrderedGraphPermitRelease,
            Self::OrderedGraphPermitCommit { .. } => RequestKind::OrderedGraphPermitCommit,
            Self::OrderedGraphPermitForget { .. } => RequestKind::OrderedGraphPermitForget,
            Self::OrderedMarkerWrite { .. } => RequestKind::OrderedMarkerWrite,
            Self::OrderedMarkerWriteSettlement { .. } => RequestKind::OrderedMarkerWriteSettlement,
            Self::OrderedCancelMarkerWrite { .. } => RequestKind::OrderedCancelMarkerWrite,
            Self::OrderedForgetMarkerWriteReceipt { .. } => {
                RequestKind::OrderedForgetMarkerWriteReceipt
            }
            Self::OrderedAcknowledgeBatch { .. } => RequestKind::OrderedAcknowledgeBatch,
            Self::OrderedClose { .. } => RequestKind::OrderedClose,
            Self::OrderedSeal { .. } => RequestKind::OrderedSeal,
            Self::OrderedRetainClaim { .. } => RequestKind::OrderedRetainClaim,
            Self::OrderedConfirmCleanup { .. } => RequestKind::OrderedConfirmCleanup,
            Self::OrderedRemove { .. } => RequestKind::OrderedRemove,
            Self::OrderedForgetRemovalReceipt { .. } => RequestKind::OrderedForgetRemovalReceipt,
            Self::ReadMtu { .. } => RequestKind::ReadMtu,
            Self::ReadWriteLimits { .. } => RequestKind::ReadWriteLimits,
            Self::ReadWriteReadiness { .. } => RequestKind::ReadWriteReadiness,
            Self::RequestMtu { .. } => RequestKind::RequestMtu,
            Self::ReadRssi { .. } => RequestKind::ReadRssi,
            Self::RequestConnectionPriority { .. } => RequestKind::RequestConnectionPriority,
            Self::RequestSubrate { .. } => RequestKind::RequestSubrate,
            Self::ReadPhy { .. } => RequestKind::ReadPhy,
            Self::RequestPhy { .. } => RequestKind::RequestPhy,
            Self::SecurityState { .. } => RequestKind::SecurityState,
            Self::CreateBond { .. } => RequestKind::CreateBond,
            Self::CancelBond { .. } => RequestKind::CancelBond,
            Self::BondedPeers { .. } => RequestKind::BondedPeers,
            Self::ConnectedPeers { .. } => RequestKind::ConnectedPeers,
            Self::ResolvePeer { .. } => RequestKind::ResolvePeer,
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
    OwnerRetirement(OwnerRetirementResult),
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
    DiscoveredScoped {
        services: Vec<ServiceSnapshot>,
        identity: ubm_desktop::boundary::GattSnapshotIdentity,
    },
    NotifyEnabled(ObservedDelivery),
    OrderedEnableReleased(PlatformFailure),
    OrderedEnableStatus(OrderedEnableStatus),
    OrderedOpened {
        generation: OrderedSourceGeneration,
        members: Vec<OrderedRegistration>,
    },
    OrderedOpenV2(OrderedOpenCompletionV2),
    OrderedOpenFailed {
        failure: PlatformFailure,
        ownership: OrderedOpenOwnership,
    },
    OrderedRegistered(OrderedRegistration),
    OrderedClaim {
        generation: OrderedSourceGeneration,
        member: OrderedRegistration,
    },
    OrderedGraphPermitReserved(u64),
    OrderedGraphAuthorityReserved(u64),
    OrderedGraphReserveStatus(OrderedGraphReserveStatus),
    OrderedGraphAuthorityOpened(u64),
    OrderedGraphAuthorityStatus(OrderedGraphAuthorityStatus),
    OrderedGraphPermit(OrderedGraphPermitState),
    OrderedGraphPermitInvalidated(PlatformFailure),
    OrderedGraphPermitNotAcquired(PlatformFailure),
    OrderedMarkerWriteStatus(OrderedMarkerWriteStatus),
    OrderedRecords {
        batch_id: Option<u64>,
        records: Vec<OrderedRecord>,
    },
    Mtu(Option<u16>),
    /// Per-mode single-write limits; both are at least one byte.
    WriteLimits(WriteLimits),
    /// `canSendWriteWithoutResponse` for [`RequestKind::ReadWriteReadiness`].
    Ready(bool),
    Rssi(i16),
    Accepted(bool),
    Phy(PhyObservation),
    PhyRequest {
        accepted: bool,
        observation: Option<PhyObservation>,
    },
    Security(SecurityState),
    BondedPeers(Vec<BondedPeer>),
    ConnectedPeers(Vec<ConnectedPeer>),
    ResolvedPeer(Option<ResolvedPeer>),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedGraphReserveStatus {
    Pending,
    NotCreated,
    Issued(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedGraphAuthorityStatus {
    Pending,
    NotCreated,
    Opened(u64),
    Closed(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedGraphPermitState {
    Acquired,
    Stale,
    Busy,
    Committed,
    Released,
    Forgotten,
}

impl RadioCompletion {
    /// Validate a response against the full request when identity matters.
    /// Generic failures report an error and never constitute retirement.
    #[must_use]
    pub fn answers_request(&self, request: &RadioRequest) -> bool {
        match (self, request) {
            (
                Self::OwnerRetirement(result),
                RadioRequest::OwnerFence {
                    owner_incarnation,
                    retirement_id,
                    ..
                }
                | RadioRequest::OwnerStatus {
                    owner_incarnation,
                    retirement_id,
                    ..
                },
            ) => {
                result.owner_incarnation == *owner_incarnation
                    && result.retirement_id == *retirement_id
            }
            (_, RadioRequest::OwnerFence { .. } | RadioRequest::OwnerStatus { .. }) => {
                matches!(self, Self::Failed(_))
            }
            (Self::OrderedOpenV2(completion), RadioRequest::OrderedOpenV2(request)) => {
                completion.answers(request)
            }
            (_, RadioRequest::OrderedOpenV2(_)) => matches!(self, Self::Failed(_)),
            _ => self.answers(request.kind()),
        }
    }
    /// Whether this completion has the documented shape for `kind`.
    /// This is insufficient for v2 slot identity: use `answers_request` before
    /// adopting a result or confirming retirement. A failure is an error,
    /// never proof of no effect or retirement.
    #[must_use]
    pub fn answers(&self, kind: RequestKind) -> bool {
        use RequestKind as K;
        // A kind alone cannot establish which owner and retirement answered.
        // ForeignRadio must retain the exact request and use answers_request
        // before v4 completion admission can be enabled.
        if matches!(self, Self::OwnerRetirement(_)) {
            return false;
        }
        if let (
            Self::OrderedMarkerWriteStatus(status),
            K::OrderedMarkerWrite | K::OrderedMarkerWriteSettlement | K::OrderedCancelMarkerWrite,
        ) = (self, kind)
        {
            return matches!(
                (&status.admission, &status.write),
                (
                    OrderedMarkerAdmission::Pending
                        | OrderedMarkerAdmission::NotAdmitted
                        | OrderedMarkerAdmission::Rejected { .. },
                    OrderedMarkerWriteState::NotStarted { .. }
                ) | (
                    OrderedMarkerAdmission::Admitted { .. },
                    OrderedMarkerWriteState::Initiating
                        | OrderedMarkerWriteState::NotStarted { .. }
                        | OrderedMarkerWriteState::Started { .. }
                        | OrderedMarkerWriteState::Settled { .. }
                )
            );
        }
        if let (Self::WriteLimits(limits), K::ReadWriteLimits) = (self, kind) {
            return limits.with_response > 0 && limits.without_response > 0;
        }
        if let (Self::OrderedRecords { batch_id, records }, K::OrderedDrain) = (self, kind) {
            return batch_id.is_some() != records.is_empty();
        }
        matches!(
            (self, kind),
            (
                Self::OrderedOpenV2(OrderedOpenCompletionV2::Result(_)),
                K::OrderedOpenV2 | K::OrderedRecoverOpenV2
            ) | (
                Self::OrderedOpenV2(OrderedOpenCompletionV2::Retired(_)),
                K::OrderedAcknowledgeOpenV2
            ) | (
                Self::OrderedGraphPermitReserved(_),
                K::OrderedGraphPermitReserve
            ) | (
                Self::OrderedGraphReserveStatus(_),
                K::OrderedGraphPermitReserve
                    | K::OrderedGraphPermitReserveFence
                    | K::OrderedGraphPermitReserveStatus
                    | K::OrderedGraphAuthorityReserve
                    | K::OrderedGraphAuthorityReserveFence
                    | K::OrderedGraphAuthorityReserveStatus
            ) | (
                Self::OrderedGraphAuthorityReserved(_),
                K::OrderedGraphAuthorityReserve
            ) | (
                Self::OrderedGraphAuthorityOpened(_),
                K::OrderedGraphAuthorityOpen
            ) | (
                Self::OrderedGraphAuthorityStatus(_),
                K::OrderedGraphAuthorityOpen
                    | K::OrderedGraphAuthorityStatus
                    | K::OrderedGraphAuthorityFenceOpen
            ) | (
                Self::OrderedGraphPermit(
                    OrderedGraphPermitState::Acquired
                        | OrderedGraphPermitState::Stale
                        | OrderedGraphPermitState::Busy
                ),
                K::OrderedGraphPermitAcquire
            ) | (
                Self::OrderedGraphPermitNotAcquired(_),
                K::OrderedGraphPermitAcquire
            ) | (
                Self::OrderedGraphPermit(OrderedGraphPermitState::Forgotten),
                K::OrderedGraphPermitForget
            ) | (
                Self::OrderedGraphPermit(OrderedGraphPermitState::Released),
                K::OrderedGraphPermitRelease
            ) | (
                Self::OrderedGraphPermit(OrderedGraphPermitState::Committed)
                    | Self::OrderedGraphPermitInvalidated(_),
                K::OrderedGraphPermitCommit
            ) | (Self::Failed(_), _)
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
                        | K::OrderedGraphPermitForgetRequest
                        | K::OrderedGraphAuthorityReserveForget
                        | K::OrderedGraphAuthorityForgetOpen
                        | K::OrderedGraphAuthorityClose
                        | K::OrderedGraphAuthorityForget
                        | K::OrderedRetire
                        | K::OrderedBarrier
                        | K::OrderedForgetMarkerWriteReceipt
                        | K::OrderedAcknowledgeOpen
                        | K::OrderedAcknowledgeBatch
                        | K::OrderedClose
                        | K::OrderedSeal
                        | K::OrderedConfirmCleanup
                        | K::OrderedRemove
                        | K::OrderedForgetRemovalReceipt
                        | K::OrderedForgetEnableReceipt
                        | K::OrderedDisableNotifications
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
                | (Self::DiscoveredScoped { .. }, K::Discover)
                | (Self::Read { .. }, K::Read)
                | (Self::Bytes(_), K::ReadDescriptor)
                | (Self::NotifyEnabled(_), K::EnableNotifications)
                | (Self::NotifyEnabled(_), K::OrderedEnableNotifications)
                | (
                    Self::OrderedEnableReleased(_),
                    K::OrderedEnableNotifications
                )
                | (Self::OrderedEnableStatus(_), K::OrderedEnableSettlement)
                | (
                    Self::OrderedOpened { .. },
                    K::OrderedOpen | K::OrderedRecoverOpen
                )
                | (
                    Self::OrderedOpenFailed { .. },
                    K::OrderedOpen | K::OrderedRecoverOpen
                )
                | (Self::OrderedRegistered(_), K::OrderedRegister)
                | (Self::OrderedClaim { .. }, K::OrderedRetainClaim)
                | (
                    Self::OrderedMarkerWriteStatus(_),
                    K::OrderedMarkerWrite
                        | K::OrderedMarkerWriteSettlement
                        | K::OrderedCancelMarkerWrite
                )
                | (Self::OrderedRecords { .. }, K::OrderedDrain)
                | (Self::Mtu(_), K::ReadMtu)
                | (Self::WriteLimits(_), K::ReadWriteLimits)
                | (Self::Ready(_), K::ReadWriteReadiness)
                | (Self::Mtu(Some(_)), K::RequestMtu)
                | (Self::Rssi(_), K::ReadRssi)
                | (Self::Accepted(_), K::RequestConnectionPriority)
                | (Self::Accepted(_), K::RequestSubrate)
                | (Self::Phy(_), K::ReadPhy)
                | (Self::PhyRequest { .. }, K::RequestPhy)
                | (Self::Security(_), K::SecurityState | K::CreateBond)
                | (Self::BondedPeers(_), K::BondedPeers)
                | (Self::ConnectedPeers(_), K::ConnectedPeers)
                | (Self::ResolvedPeer(_), K::ResolvePeer)
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
    pub capture_timestamp_ms: Option<u64>,
    pub cached_name: Option<String>,
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
pub use ubm_desktop::continuation_outbox::IngressClass;

/// One unsolicited platform fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioIngress {
    Advertisement(Advertisement),
    /// An advertisement tagged with the exact `StartScan` request id that
    /// installed its native callback. The native bridge must echo that id.
    AdvertisementScoped {
        scan_request_id: RequestId,
        advertisement: Advertisement,
    },
    OrderedWake {
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    OrderedWakeFailure {
        generation: OrderedSourceGeneration,
        group_id: u64,
    },
    /// Link change the OS reported (`status` = platform GATT status).
    Connection {
        peer_id: String,
        connected: bool,
        status: Option<i32>,
    },
    /// Link change scoped to the core generation that admitted it.
    ConnectionScoped {
        peer_id: String,
        expected_generation: String,
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
    /// The native notification source stopped for this enable generation.
    NotificationSourceFailed {
        instance: Instance,
        epoch: u64,
        failure: PlatformFailure,
    },
    AdapterState(AdapterSnapshot),
    /// The OS stopped the scan on its own (Android `onScanFailed`).
    ScanFailed {
        detail: String,
    },
    /// Terminal callback for the exact `StartScan` request which installed it.
    ScanFailedScoped {
        scan_request_id: RequestId,
        detail: String,
    },
    /// Android's native scan callback, including its integer error code.
    ScanFailedAndroid {
        scan_request_id: RequestId,
        detail: String,
        native_code: i32,
    },
    SecurityChanged {
        peer_id: String,
        state: SecurityState,
    },
    SecurityFailed {
        peer_id: Option<String>,
        failure: PlatformFailure,
    },
    /// CoreBluetooth `peripheralIsReady(toSendWriteWithoutResponse:)`.
    /// `ready` is the queue state after that callback, which Apple documents
    /// as able to accept another write without response.
    WriteReadiness {
        peer_id: String,
        ready: bool,
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
    /// An exact batch of observations dropped at one native source.
    DroppedCount {
        class: IngressClass,
        detail: String,
        count: u64,
    },
}

/// Admission of a notification source failure into the Rust ingress FIFO.
///
/// Only `Accepted` transfers responsibility to the host. On `Full`, `Closed`
/// or `Invalid`, the caller retains the exact owner and failure. Accepted
/// terminals are enqueued through the dedicated source-failure FIFO path, so
/// queue pressure never reclassifies the source terminal as a counted drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SourceFailureIngressStatus {
    Accepted,
    Full,
    Closed,
    Invalid,
}

/// What [`crate::MobileHost::ingest`] did with one fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressStatus {
    Accepted,
    /// A bounded queue was full; the drop is counted and surfaced as an
    /// `ingress-drop` record.
    Dropped(IngressClass),
    /// Scan failure admission was refused; ownership remains with the caller.
    ScanFailureRefused {
        error: ubm_desktop::DesktopError,
        closed: bool,
    },
    /// The host is shut down.
    Closed,
}

impl IngressStatus {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Dropped(IngressClass::Advertisement) => "dropped-advertisement",
            Self::Dropped(IngressClass::Notification) => "dropped-notification",
            Self::Dropped(IngressClass::Control) => "dropped-control",
            Self::ScanFailureRefused { closed: true, .. } => "closed",
            Self::ScanFailureRefused { closed: false, .. } => "dropped-control",
            Self::Closed => "closed",
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::{FailureKind, MobilePlatform, PlatformFailure, RequestKind};

    #[test]
    fn owner_retirement_requires_v4_and_exact_identity_across_retry() {
        use super::*;
        let owner =
            OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let other =
            OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abd")
                .unwrap();
        let retirement =
            OrderedAdmissionOwner::negotiate_slot(2, "22345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let fence = RadioRequest::OwnerFence {
            id: 10,
            owner_incarnation: owner,
            retirement_id: retirement,
        };
        let retry = RadioRequest::OwnerStatus {
            id: 11,
            owner_incarnation: owner,
            retirement_id: retirement,
        };
        assert!(fence.validate_owner_retirement(3, owner).is_err());
        assert!(fence.validate_owner_retirement(4, other).is_err());
        assert!(fence.validate_owner_retirement(4, owner).is_ok());
        assert!(
            RadioRequest::OwnerFence {
                id: 0,
                owner_incarnation: owner,
                retirement_id: retirement
            }
            .validate_owner_retirement(4, owner)
            .is_err()
        );
        assert!(OrderedAdmissionNegotiation::negotiate(4, "not-a-uuid").is_err());
        let receipt = RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation: owner,
            retirement_id: retirement,
            state: OwnerRetirementState::Quiescent,
        });
        assert!(receipt.answers_request(&fence));
        assert!(receipt.answers_request(&retry));
        assert!(!receipt.answers(RequestKind::OwnerFence));
        let wrong = RadioCompletion::OwnerRetirement(OwnerRetirementResult {
            owner_incarnation: other,
            retirement_id: retirement,
            state: OwnerRetirementState::Quiescent,
        });
        assert!(!wrong.answers_request(&retry));
    }

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
    fn notification_source_failure_does_not_fabricate_link_loss() {
        let failure = PlatformFailure {
            gatt_status: Some(8),
            native_name: Some("NOTIFICATION_DELIVERY_UNCERTAIN".into()),
            ..PlatformFailure::new(FailureKind::GattStatus, "notification delivery uncertain")
        };
        let error = failure.to_notification_source_error(MobilePlatform::Android);
        assert_eq!(error.code_str(), "platform.failure");
        assert_eq!(error.operation(), "gatt.notification.source");
        assert_eq!(
            error.platform().unwrap().code,
            "NOTIFICATION_DELIVERY_UNCERTAIN"
        );
        assert_eq!(
            error.platform().unwrap().metadata.get("androidGattStatus"),
            Some(&super::PlatformValue::Int(8))
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

#[cfg(test)]
mod ordered_admission_identity_tests {
    use super::OrderedAdmissionOwner;

    #[test]
    fn owner_handshake_accepts_only_supported_canonical_random_uuid() {
        let first =
            OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abc").unwrap();
        assert_eq!(
            first,
            OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abc").unwrap()
        );
        assert_ne!(
            first,
            OrderedAdmissionOwner::negotiate(1, "12345678-1234-4abc-8def-123456789abd").unwrap()
        );
        for (version, id) in [
            (2, "12345678-1234-4abc-8def-123456789abc"),
            (1, ""),
            (1, "00000000-0000-0000-0000-000000000000"),
            (1, "12345678-1234-4ABC-8DEF-123456789ABC"),
            (1, "12345678-1234-1abc-8def-123456789abc"),
            (1, "12345678-1234-4abc-0def-123456789abc"),
        ] {
            assert!(
                OrderedAdmissionOwner::negotiate(version, id).is_err(),
                "accepted {version}: {id}"
            );
        }
    }
}

#[cfg(test)]
mod active_slot_transport_tests {
    use super::*;

    #[test]
    fn v2_requests_require_matching_typed_completions() {
        let owner =
            OrderedAdmissionOwner::negotiate_slot(2, "12345678-1234-4abc-8def-123456789abc")
                .unwrap();
        let identity = OrderedOpenIdentity {
            version: 2,
            owner_incarnation: owner,
            slot: 3,
            slot_generation: 7,
            original_request_id: 10,
            native_group_id: 12,
            peer_id: "peer".into(),
            spec: OrderedOpenSpec {
                connection_generation: "c".into(),
                database_generation: "d".into(),
                snapshot_identity: ubm_desktop::boundary::GattSnapshotIdentity {
                    owner: "o".into(),
                    attachment: 1,
                    revision: 2,
                },
                members: vec![],
                item_capacity: 2,
                byte_capacity: 32,
            },
        };
        let open = RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Open {
            id: 10,
            identity: identity.clone(),
        });
        let recover = RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Recover {
            id: 11,
            identity: identity.clone(),
        });
        let ack = RadioRequest::OrderedOpenV2(OrderedOpenRequestV2::Acknowledge {
            id: 12,
            identity: identity.clone(),
        });
        let pending =
            RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Result(OrderedOpenResultV2 {
                slot_identity: identity.slot_identity(),
                outcome: OrderedOpenOutcomeV2::Pending,
            }));
        let retired =
            RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Retired(OrderedOpenRetired {
                slot_identity: identity.slot_identity(),
            }));
        assert_eq!((open.id(), recover.id(), ack.id()), (10, 11, 12));
        assert!(pending.answers_request(&open));
        assert!(pending.answers_request(&recover));
        assert!(!pending.answers_request(&ack));
        assert!(retired.answers_request(&ack));
        assert!(!retired.answers_request(&recover));
        assert!(!RadioCompletion::Unit.answers_request(&ack));
        let wrong =
            RadioCompletion::OrderedOpenV2(OrderedOpenCompletionV2::Retired(OrderedOpenRetired {
                slot_identity: OrderedSlotIdentity {
                    slot_generation: 8,
                    ..identity.slot_identity()
                },
            }));
        assert!(!wrong.answers_request(&ack));
        assert!(RadioCompletion::Unit.answers(RequestKind::OrderedAcknowledgeOpen));
    }

    #[test]
    fn negotiated_version_is_not_erased() {
        let id = "12345678-1234-4abc-8def-123456789abc";
        let v1 = OrderedAdmissionNegotiation::negotiate(1, id).unwrap();
        let v2 = OrderedAdmissionNegotiation::negotiate(2, id).unwrap();
        assert_eq!(v1.owner, v2.owner);
        assert_eq!((v1.version, v2.version), (1, 2));
        assert!(OrderedAdmissionNegotiation::negotiate(3, id).is_err());
    }
}
