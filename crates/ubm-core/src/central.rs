//! Central/GATT primitives over the ownership kernel (CORE-CENTRAL, UBM 5.0).
//!
//! Derived from C-UBM.0.1.2-DRAFT `central.ts` (scan/write/long-write/controls
//! validation), `hosts.ts` (scan/connection arbitration, transfer validation),
//! `transitions.ts` (scan-session, connection, database, subscription machines
//! plus contention rulings), `identities.ts` (attachment/peer/path/generation
//! rules), `capabilities.ts` (descriptor truth, admission), `streams.ts`
//! (bounded delivery, overflow policies), `cleanup.ts` (retained receipts),
//! `outcomes.ts` (error identities, recovery), and the BLE Rust convergence
//! plan sections 9.1-9.3 plus acceptance scenarios `OWN-03`, `OWN-04`,
//! `OWN-06`, `CLN-01`, `CLN-02`, `CLN-03`, `GATT-01`, `GATT-02`, `GATT-03`,
//! `GATT-04`, `OPS-01`, `OPS-02`, `OPS-03`, `STR-01`, `STR-02`.
//!
//! Design: this module is a validation and routing layer only. The
//! [`ownership::Kernel`](crate::ownership::Kernel) remains the one scheduler:
//! every central operation admits, dispatches, settles, cancels, or sweeps a
//! kernel operation, and radio-bound work is expressed as kernel effects in
//! the caller-provided [`EffectBatch`](crate::ownership::EffectBatch). There
//! is no Tokio, no second operation map, and no background work. Typed
//! [`CentralEffect`] records are an observation ledger (bounded, drained by
//! the host), never a scheduling decision.
//!
//! Failure discipline: every fallible path fails closed with a frozen
//! contract error identity ([`CoreError`] carrying [`BleErrorCode`] plus
//! [`BleErrorDomain`]). Validation runs before any kernel mutation and before
//! any effect is staged, so a rejected request leaves state unchanged and
//! emits no radio effect (OWN-02, stale-path vectors).

use std::collections::VecDeque;

use crate::contracts::{
    AttachmentTuple, BleErrorCode, BleErrorDomain, Contender, ContenderKind, CoreError, Generation,
    HandshakeState, LeaseId, MonotonicTime, OperationId, OperationTerminalKind, PeerIdentity,
    PeerIdentityDomain, assert_same_attachment, assert_timeout_ms, effective_max_bytes,
};
use crate::ownership::{
    CleanupFailure, CleanupRecord, CleanupState, EffectBatch, HandleOutcome, Kernel, KernelConfig,
    KernelInput, OpStateView, OwnershipDecision, arbitrate_connection_request,
    arbitrate_scan_request,
};
use crate::streams::{
    OverflowPolicy, RESERVED_CONTROL_BYTES, RESERVED_CONTROL_CAPACITY, Stream, StreamLimits,
};

/// Build a closed failure with a frozen identity. Never invents codes: every
/// call site names a contract path.
fn err(code: BleErrorCode, domain: BleErrorDomain, operation: &str) -> CoreError {
    CoreError::new(code, domain, operation)
}

fn append_u64(into: &mut String, mut value: u64) {
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits: [u8; 20] = [0; 20];
    let mut count = 0usize;
    while value > 0 && count < digits.len() {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    while count > 0 {
        count -= 1;
        into.push(digits[count] as char);
    }
}

/// Canonicalize a UUID exactly like C-UBM `canonicalUuidValue`: strip dashes,
/// lowercase, require hex, expand 16-bit and 32-bit forms into the Bluetooth
/// base UUID, re-dash 128-bit forms. Anything else fails closed with
/// `argument.invalid`.
pub fn canonical_uuid(value: &str) -> Result<String, CoreError> {
    if value.is_empty() {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "uuid.input",
        ));
    }
    let mut compact = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte == b'-' {
            continue;
        }
        if byte.is_ascii_uppercase() {
            compact.push((byte + 32) as char);
        } else {
            compact.push(byte as char);
        }
    }
    if compact.is_empty() {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "uuid.digits",
        ));
    }
    for byte in compact.bytes() {
        let hex = byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        if !hex {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "uuid.digits",
            ));
        }
    }
    if compact.len() == 4 {
        let mut out = String::with_capacity(36);
        out.push_str("0000");
        out.push_str(&compact);
        out.push_str("-0000-1000-8000-00805f9b34fb");
        return Ok(out);
    }
    if compact.len() == 8 {
        let mut out = String::with_capacity(36);
        out.push_str(&compact);
        out.push_str("-0000-1000-8000-00805f9b34fb");
        return Ok(out);
    }
    if compact.len() != 32 {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "uuid.length",
        ));
    }
    let bytes = compact.as_bytes();
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if index == 8 || index == 12 || index == 16 || index == 20 {
            out.push('-');
        }
        out.push(*byte as char);
    }
    Ok(out)
}

/// Canonical 128-bit form of the CCCD (descriptor `0x2902`). Direct writes to
/// this descriptor must go through subscribe/unsubscribe and fail with
/// `gatt.cccd-managed`.
pub const CCCD_UUID: &str = "00002902-0000-1000-8000-00805f9b34fb";

/// GATT property flags carried on stored paths. Discovery snapshots declare
/// them; reads, writes, and subscriptions enforce them, failing closed with
/// `gatt.property-not-supported`.
pub const GATT_PROP_READ: u8 = 0x01;
/// Write with response (`write`).
pub const GATT_PROP_WRITE: u8 = 0x02;
/// Write without response (`write-command`).
pub const GATT_PROP_WRITE_NO_RESPONSE: u8 = 0x04;
/// Notifications.
pub const GATT_PROP_NOTIFY: u8 = 0x08;
/// Indications.
pub const GATT_PROP_INDICATE: u8 = 0x10;

/// Central configuration. All bounds are explicit; zero bounds admit nothing
/// and are rejected instead of silently wedging the layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CentralConfig {
    /// Maximum concurrent connection records.
    pub max_connections: usize,
    /// Maximum cached discovered peer identities (F16: independent of the
    /// connection bound; unreferenced discoveries evict under pressure).
    pub max_discovered_peers: usize,
    /// Maximum stored GATT paths.
    pub max_paths: usize,
    /// Maximum subscription hubs (one per subscribed path).
    pub max_subscriptions: usize,
    /// Maximum logical consumers sharing one physical enablement.
    pub max_consumers_per_subscription: usize,
    /// Maximum retained typed observation effects.
    pub typed_effect_cap: usize,
    /// Kernel admission bounds (the one scheduler's budgets).
    pub kernel: KernelConfig,
}

impl Default for CentralConfig {
    fn default() -> Self {
        Self {
            max_connections: 16,
            max_discovered_peers: 16,
            max_paths: 128,
            max_subscriptions: 32,
            max_consumers_per_subscription: 8,
            typed_effect_cap: 256,
            kernel: KernelConfig::default(),
        }
    }
}

impl CentralConfig {
    /// Validate and build a configuration.
    pub fn new(
        max_connections: usize,
        max_discovered_peers: usize,
        max_paths: usize,
        max_subscriptions: usize,
        max_consumers_per_subscription: usize,
        typed_effect_cap: usize,
        kernel: KernelConfig,
    ) -> Result<Self, CoreError> {
        if max_connections == 0
            || max_discovered_peers == 0
            || max_paths == 0
            || max_subscriptions == 0
            || max_consumers_per_subscription == 0
            || typed_effect_cap == 0
        {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "central.config.bounds",
            ));
        }
        Ok(Self {
            max_connections,
            max_discovered_peers,
            max_paths,
            max_subscriptions,
            max_consumers_per_subscription,
            typed_effect_cap,
            kernel,
        })
    }
}

/// Scan duplicate delivery policy, verbatim from C-UBM `ScanDuplicatePolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanDuplicatePolicy {
    All,
    First,
    Merged,
}

impl ScanDuplicatePolicy {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::First => "first",
            Self::Merged => "merged",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown policies.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "all") {
            Some(Self::All)
        } else if string_eq(value, "first") {
            Some(Self::First)
        } else if string_eq(value, "merged") {
            Some(Self::Merged)
        } else {
            None
        }
    }
}

const fn string_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Scan observation merge policy, verbatim from C-UBM `ScanMergePolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanMergePolicy {
    None,
    LatestByTimestamp,
}

impl ScanMergePolicy {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LatestByTimestamp => "latest-by-timestamp",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown policies.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "none") {
            Some(Self::None)
        } else if string_eq(value, "latest-by-timestamp") {
            Some(Self::LatestByTimestamp)
        } else {
            None
        }
    }
}

/// Validated scan request. Empty service filters mean the platform's broad
/// scan; unsupported filter fields fail instead of broadening or narrowing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRequest {
    service_uuids: Vec<String>,
    duplicate: ScanDuplicatePolicy,
    merge: ScanMergePolicy,
    timeout_ms: u64,
    has_abort_signal: bool,
}

impl ScanRequest {
    /// Canonical service filters.
    #[must_use]
    pub fn service_uuids(&self) -> &[String] {
        &self.service_uuids
    }

    /// Duplicate policy.
    #[must_use]
    pub const fn duplicate(&self) -> ScanDuplicatePolicy {
        self.duplicate
    }

    /// Merge policy.
    #[must_use]
    pub const fn merge(&self) -> ScanMergePolicy {
        self.merge
    }

    /// Deadline budget in milliseconds.
    #[must_use]
    pub const fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Whether the caller holds an abort signal.
    #[must_use]
    pub const fn has_abort_signal(&self) -> bool {
        self.has_abort_signal
    }
}

/// Validate a scan request, mirroring C-UBM `validateScanRequest`: residuals
/// fail as `capability.unsupported`, bad UUIDs/policies/timeouts as
/// `argument.invalid`, all before any radio effect.
pub fn validate_scan_request(
    service_uuids: &[&str],
    duplicate_policy: &str,
    merge_policy: &str,
    timeout_ms: u64,
    has_abort_signal: bool,
    unsupported_filter_fields: &[&str],
) -> Result<ScanRequest, CoreError> {
    if !unsupported_filter_fields.is_empty() {
        return Err(err(
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
            "scan.filter",
        ));
    }
    let mut canonical: Vec<String> = Vec::with_capacity(service_uuids.len());
    for uuid in service_uuids {
        canonical.push(canonical_uuid(uuid)?);
    }
    let Some(duplicate) = ScanDuplicatePolicy::from_str(duplicate_policy) else {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "scan.duplicate-policy",
        ));
    };
    let Some(merge) = ScanMergePolicy::from_str(merge_policy) else {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "scan.merge-policy",
        ));
    };
    let timeout_ms = assert_timeout_ms(timeout_ms, "scan.timeout")?;
    Ok(ScanRequest {
        service_uuids: canonical,
        duplicate,
        merge,
        timeout_ms,
        has_abort_signal,
    })
}

/// Scan-session lifecycle, verbatim from the frozen `scan-session` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanSessionState {
    Starting,
    Active,
    Stopping,
    Stopped,
    Failed,
}

impl ScanSessionState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    /// Whether this state is terminal (`stopped`, `failed`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

/// Platform event settling a scan session, named after the frozen `via`
/// labels of the `scan-session` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanPlatformEvent {
    PlatformStarted,
    Stop,
    PlatformStopped,
    SourceClosed,
    StartFailed,
    SourceFailed,
    OverflowErrorPolicy,
    Reset,
    StopFailed,
}

/// Step one scan session through the frozen `scan-session` table. Anything
/// off-table fails closed with `lifecycle.invalid-state`; terminals never
/// leave.
pub fn step_scan_session(
    from: ScanSessionState,
    event: ScanPlatformEvent,
) -> Result<ScanSessionState, CoreError> {
    match (from, event) {
        (ScanSessionState::Starting, ScanPlatformEvent::PlatformStarted) => {
            Ok(ScanSessionState::Active)
        }
        (ScanSessionState::Starting, ScanPlatformEvent::Stop) => Ok(ScanSessionState::Stopping),
        (ScanSessionState::Starting, ScanPlatformEvent::StartFailed) => {
            Ok(ScanSessionState::Failed)
        }
        (ScanSessionState::Active, ScanPlatformEvent::Stop) => Ok(ScanSessionState::Stopping),
        (ScanSessionState::Active, ScanPlatformEvent::SourceClosed) => {
            Ok(ScanSessionState::Stopped)
        }
        (ScanSessionState::Active, ScanPlatformEvent::SourceFailed)
        | (ScanSessionState::Active, ScanPlatformEvent::OverflowErrorPolicy)
        | (ScanSessionState::Active, ScanPlatformEvent::Reset) => Ok(ScanSessionState::Failed),
        (ScanSessionState::Stopping, ScanPlatformEvent::PlatformStopped) => {
            Ok(ScanSessionState::Stopped)
        }
        (ScanSessionState::Stopping, ScanPlatformEvent::StopFailed) => Ok(ScanSessionState::Failed),
        _ => Err(err(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            "central.scan.transition",
        )),
    }
}

/// Connection lifecycle, verbatim from the frozen `connection` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
    Lost,
    Invalid,
}

impl ConnectionState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Disconnecting => "disconnecting",
            Self::Disconnected => "disconnected",
            Self::Lost => "lost",
            Self::Invalid => "invalid",
        }
    }

    /// Whether this state is terminal (`disconnected`, `lost`, `invalid`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Disconnected | Self::Lost | Self::Invalid)
    }
}

/// Connection event, named after the frozen `via` labels of the `connection`
/// table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionEvent {
    LinkEstablished,
    Disconnect,
    PeerLoss,
    Reset,
    LinkReleased,
}

/// Step one connection through the frozen `connection` table. A second
/// terminal event never resurrects or double-releases the link (CLN-02): it
/// fails closed with `lifecycle.invalid-state` and changes nothing.
pub fn step_connection(
    from: ConnectionState,
    event: ConnectionEvent,
) -> Result<ConnectionState, CoreError> {
    match (from, event) {
        (ConnectionState::Connecting, ConnectionEvent::LinkEstablished) => {
            Ok(ConnectionState::Connected)
        }
        (ConnectionState::Connecting, ConnectionEvent::Disconnect) => {
            Ok(ConnectionState::Disconnecting)
        }
        (ConnectionState::Connecting, ConnectionEvent::PeerLoss) => Ok(ConnectionState::Lost),
        (ConnectionState::Connecting, ConnectionEvent::Reset) => Ok(ConnectionState::Invalid),
        (ConnectionState::Connected, ConnectionEvent::Disconnect) => {
            Ok(ConnectionState::Disconnecting)
        }
        (ConnectionState::Connected, ConnectionEvent::PeerLoss) => Ok(ConnectionState::Lost),
        (ConnectionState::Connected, ConnectionEvent::Reset) => Ok(ConnectionState::Invalid),
        (ConnectionState::Disconnecting, ConnectionEvent::LinkReleased) => {
            Ok(ConnectionState::Disconnected)
        }
        (ConnectionState::Disconnecting, ConnectionEvent::PeerLoss) => Ok(ConnectionState::Lost),
        (ConnectionState::Disconnecting, ConnectionEvent::Reset) => Ok(ConnectionState::Invalid),
        _ => Err(err(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            "central.connection.transition",
        )),
    }
}

/// GATT database lifecycle, verbatim from the frozen `database` table. A
/// service change or new discovery never revives a stale snapshot: recovery
/// mints a new generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseState {
    Undiscovered,
    Discovering,
    Current,
    Changed,
    Invalid,
}

impl DatabaseState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Undiscovered => "undiscovered",
            Self::Discovering => "discovering",
            Self::Current => "current",
            Self::Changed => "changed",
            Self::Invalid => "invalid",
        }
    }
}

/// Database event, named after the frozen `via` labels of the `database`
/// table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DatabaseEvent {
    Discover,
    Rediscover,
    SnapshotComplete,
    DiscoveryFailed,
    ConnectionLoss,
    ServicesChanged,
    RequireRediscovery,
}

/// Step one database through the frozen `database` table.
pub fn step_database(
    from: DatabaseState,
    event: DatabaseEvent,
) -> Result<DatabaseState, CoreError> {
    match (from, event) {
        (DatabaseState::Undiscovered, DatabaseEvent::Discover) => Ok(DatabaseState::Discovering),
        (DatabaseState::Current, DatabaseEvent::Rediscover) => Ok(DatabaseState::Discovering),
        (DatabaseState::Discovering, DatabaseEvent::SnapshotComplete) => Ok(DatabaseState::Current),
        (DatabaseState::Discovering, DatabaseEvent::DiscoveryFailed) => {
            Ok(DatabaseState::Undiscovered)
        }
        (DatabaseState::Discovering, DatabaseEvent::ConnectionLoss) => Ok(DatabaseState::Invalid),
        (DatabaseState::Current, DatabaseEvent::ServicesChanged) => Ok(DatabaseState::Changed),
        (DatabaseState::Current, DatabaseEvent::ConnectionLoss) => Ok(DatabaseState::Invalid),
        (DatabaseState::Changed, DatabaseEvent::RequireRediscovery) => {
            Ok(DatabaseState::Undiscovered)
        }
        _ => Err(err(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            "central.database.transition",
        )),
    }
}

/// Write mode, verbatim from C-UBM `WriteMode`. The mode is mandatory and is
/// never substituted: an unavailable mode fails instead of degrading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteMode {
    WithResponse,
    WithoutResponse,
    LongWrite,
}

impl WriteMode {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WithResponse => "with-response",
            Self::WithoutResponse => "without-response",
            Self::LongWrite => "long-write",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown modes.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "with-response") {
            Some(Self::WithResponse)
        } else if string_eq(value, "without-response") {
            Some(Self::WithoutResponse)
        } else if string_eq(value, "long-write") {
            Some(Self::LongWrite)
        } else {
            None
        }
    }

    /// Property flag this mode requires on the target path.
    #[must_use]
    pub const fn required_property(self) -> u8 {
        match self {
            Self::WithResponse | Self::LongWrite => GATT_PROP_WRITE,
            Self::WithoutResponse => GATT_PROP_WRITE_NO_RESPONSE,
        }
    }
}

/// Long-write segmentation plan. A sequential emulation never claims a
/// native atomic transaction (`atomic` is always false).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongWritePlan {
    segment_maximum: u64,
    segments: u64,
}

impl LongWritePlan {
    /// Largest payload per segment: the minimum of the stacked limits.
    #[must_use]
    pub const fn segment_maximum(&self) -> u64 {
        self.segment_maximum
    }

    /// Number of sequential segments (`0` for an empty value).
    #[must_use]
    pub const fn segments(&self) -> u64 {
        self.segments
    }

    /// Always false: sequential emulation is not an atomic transaction.
    #[must_use]
    pub const fn atomic(&self) -> bool {
        false
    }
}

/// Plan a long write, mirroring C-UBM `planLongWrite`: the segment maximum is
/// the minimum of the effective operation payload limit, the negotiated
/// directional limit, and the declared backend limit. A missing limit is
/// `capability.unavailable`, never infinity; a non-positive limit is
/// `argument.invalid`.
pub fn plan_long_write(
    value_byte_length: u64,
    operation_payload_limit: Option<u64>,
    negotiated_directional_limit: Option<u64>,
    backend_limit: Option<u64>,
) -> Result<LongWritePlan, CoreError> {
    let mut segment_maximum: Option<u64> = None;
    let limits = [
        (operation_payload_limit, "long-write.maximum"),
        (negotiated_directional_limit, "long-write.maximum"),
        (backend_limit, "long-write.maximum"),
    ];
    for (limit, _operation) in limits {
        let Some(bound) = limit else {
            return Err(err(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                "long-write.maximum",
            ));
        };
        if bound == 0 {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "long-write.maximum",
            ));
        }
        segment_maximum = Some(match segment_maximum {
            Some(current) if current < bound => current,
            _ => bound,
        });
    }
    let Some(segment_maximum) = segment_maximum else {
        return Err(err(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
            "long-write.maximum",
        ));
    };
    let segments = match value_byte_length.checked_add(segment_maximum.saturating_sub(1)) {
        Some(total) => total / segment_maximum,
        None => {
            return Err(err(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Gatt,
                "long-write.length",
            ));
        }
    };
    Ok(LongWritePlan {
        segment_maximum,
        segments,
    })
}

/// Outcome of one executed long write. Segments settle sequentially through
/// the kernel; a failure stops the sequence with completed segments left in
/// place (no rollback is claimed, no automatic duplicate is issued — OPS-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongWriteOutcome {
    segments: u64,
    completed: u64,
    failed_at: Option<u64>,
}

impl LongWriteOutcome {
    /// Planned segment count.
    #[must_use]
    pub const fn segments(&self) -> u64 {
        self.segments
    }

    /// Segments settled successfully before the stop.
    #[must_use]
    pub const fn completed(&self) -> u64 {
        self.completed
    }

    /// Failing segment index, if the sequence stopped early.
    #[must_use]
    pub const fn failed_at(&self) -> Option<u64> {
        self.failed_at
    }

    /// Whether every planned segment completed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failed_at.is_none()
    }

    /// Caller-facing error for a partial failure: `gatt.write-failed`, never
    /// a silent partial success.
    #[must_use]
    pub fn error(&self, operation: &str) -> Option<CoreError> {
        if self.failed_at.is_some() {
            Some(err(
                BleErrorCode::GattWriteFailed,
                BleErrorDomain::Gatt,
                operation,
            ))
        } else {
            None
        }
    }
}

/// Capability state, verbatim from C-UBM `CapabilityState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityState {
    Supported,
    Limited,
    Unsupported,
    Unavailable,
}

impl CapabilityState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Limited => "limited",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown states.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "supported") {
            Some(Self::Supported)
        } else if string_eq(value, "limited") {
            Some(Self::Limited)
        } else if string_eq(value, "unsupported") {
            Some(Self::Unsupported)
        } else if string_eq(value, "unavailable") {
            Some(Self::Unavailable)
        } else {
            None
        }
    }
}

/// Capability admission decision, mirroring C-UBM `assertCapabilityAllows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityAdmission {
    Proceed,
    ProceedWithLimitation,
}

/// Evidence level, verbatim from C-UBM `EvidenceLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceLevel {
    Blocked,
    Deterministic,
    LivePreview,
    Supported,
    ReliabilityQualified,
}

impl EvidenceLevel {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Deterministic => "deterministic",
            Self::LivePreview => "live-preview",
            Self::Supported => "supported",
            Self::ReliabilityQualified => "reliability-qualified",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown levels.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "blocked") {
            Some(Self::Blocked)
        } else if string_eq(value, "deterministic") {
            Some(Self::Deterministic)
        } else if string_eq(value, "live-preview") {
            Some(Self::LivePreview)
        } else if string_eq(value, "supported") {
            Some(Self::Supported)
        } else if string_eq(value, "reliability-qualified") {
            Some(Self::ReliabilityQualified)
        } else {
            None
        }
    }
}

/// Runtime capability descriptor. Capability data is runtime information from
/// the instantiated backend, never a static platform matrix; a state other
/// than `supported` requires at least one limitation reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    id: String,
    state: CapabilityState,
    limits: Vec<(String, u64)>,
    limitations: Vec<String>,
    receipt_id: String,
    evidence_level: EvidenceLevel,
    implementation_version: String,
    source_digest: String,
    scenario_ids: Vec<String>,
}

impl CapabilityDescriptor {
    /// Validate and build a descriptor, mirroring C-UBM
    /// `makeCapabilityDescriptor`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        state: CapabilityState,
        limits: &[(&str, u64)],
        limitations: &[&str],
        receipt_id: &str,
        evidence_level: EvidenceLevel,
        implementation_version: &str,
        source_digest: &str,
        scenario_ids: &[&str],
    ) -> Result<Self, CoreError> {
        if id.is_empty() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "capability.id",
            ));
        }
        if state != CapabilityState::Supported && limitations.is_empty() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Capability,
                "capability.reason-required",
            ));
        }
        if receipt_id.is_empty() || implementation_version.is_empty() || source_digest.is_empty() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Capability,
                "capability.evidence",
            ));
        }
        let mut owned_limits: Vec<(String, u64)> = Vec::with_capacity(limits.len());
        for (key, bound) in limits {
            if (*key).is_empty() {
                return Err(err(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Capability,
                    "capability.limits",
                ));
            }
            owned_limits.push((String::from(*key), *bound));
        }
        let mut owned_limitations: Vec<String> = Vec::with_capacity(limitations.len());
        for limitation in limitations {
            if limitation.is_empty() {
                return Err(err(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Capability,
                    "capability.limitation",
                ));
            }
            owned_limitations.push(String::from(*limitation));
        }
        let mut owned_scenarios: Vec<String> = Vec::with_capacity(scenario_ids.len());
        for scenario in scenario_ids {
            if scenario.is_empty() {
                return Err(err(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Capability,
                    "capability.evidence.scenario",
                ));
            }
            owned_scenarios.push(String::from(*scenario));
        }
        Ok(Self {
            id: String::from(id),
            state,
            limits: owned_limits,
            limitations: owned_limitations,
            receipt_id: String::from(receipt_id),
            evidence_level,
            implementation_version: String::from(implementation_version),
            source_digest: String::from(source_digest),
            scenario_ids: owned_scenarios,
        })
    }

    /// Capability id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> CapabilityState {
        self.state
    }

    /// Numeric limits.
    #[must_use]
    pub fn limits(&self) -> &[(String, u64)] {
        &self.limits
    }

    /// Limitation reasons (non-empty unless `supported`).
    #[must_use]
    pub fn limitations(&self) -> &[String] {
        &self.limitations
    }

    /// Evidence receipt id.
    #[must_use]
    pub fn receipt_id(&self) -> &str {
        &self.receipt_id
    }

    /// Evidence level.
    #[must_use]
    pub const fn evidence_level(&self) -> EvidenceLevel {
        self.evidence_level
    }
}

/// Required central capability ids. The frozen required-capability matrix
/// controls completion: these rows are always reported by
/// [`Central::parity_rows`] and can never be erased by marking a capability
/// unsupported.
pub const REQUIRED_CAPABILITY_IDS: [&str; 6] = [
    "central.scan",
    "central.connect",
    "central.discover",
    "central.read",
    "central.write",
    "central.subscribe",
];

/// Connection-control method bound to its runtime capability id. Acceptance
/// of a request is dispatch acceptance only and is never proof that the
/// controller or peer selected the requested parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CentralControl {
    /// Control method name.
    pub method: &'static str,
    /// Runtime capability id gating the method.
    pub capability_id: &'static str,
    /// Always false: acceptance is not proof of selection.
    pub acceptance_is_proof: bool,
}

/// Frozen control table, verbatim from C-UBM `CENTRAL_CONTROLS` (10 rows;
/// security operations are separate typed effects, not control rows).
pub const CENTRAL_CONTROLS: [CentralControl; 10] = [
    CentralControl {
        method: "readRssi",
        capability_id: "connection:rssi",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "effectiveMtu",
        capability_id: "connection:effective-mtu",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "requestMtu",
        capability_id: "connection:request-mtu",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "requestPriority",
        capability_id: "connection:priority",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "parameters",
        capability_id: "connection:parameters",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "readPhy",
        capability_id: "connection:phy",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "requestPhy",
        capability_id: "connection:phy",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "requestSubrate",
        capability_id: "connection:subrate",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "maximumWriteLength",
        capability_id: "gatt:maximum-write-length",
        acceptance_is_proof: false,
    },
    CentralControl {
        method: "writeReadiness",
        capability_id: "gatt:write-without-response-readiness",
        acceptance_is_proof: false,
    },
];

/// Look up a control by method name. Returns `None` for unknown methods.
#[must_use]
pub const fn central_control_for(method: &str) -> Option<&'static CentralControl> {
    let mut index = 0;
    while index < CENTRAL_CONTROLS.len() {
        let control = &CENTRAL_CONTROLS[index];
        if string_eq(control.method, method) {
            return Some(control);
        }
        index += 1;
    }
    None
}

/// Typed central effect kind. Every state-changing central operation stages
/// exactly one of these into the bounded observation ledger; the kernel
/// [`EffectKind`] effects staged into the caller batch remain the only
/// dispatch mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CentralEffectKind {
    ScanStart,
    ScanSettled,
    Connect,
    Borrow,
    Transfer,
    Release,
    Disconnect,
    PeerLoss,
    AdapterReset,
    Discover,
    ServicesChanged,
    Read,
    Write,
    LongWrite,
    ReadDescriptor,
    WriteDescriptor,
    SubscribeEnable,
    SubscribeDisable,
    Control,
    SecurityPair,
    SecurityCancelPairing,
    SecurityUnpair,
    Destroy,
}

impl CentralEffectKind {
    /// Frozen wire string for this effect kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScanStart => "central.scan-start",
            Self::ScanSettled => "central.scan-settled",
            Self::Connect => "central.connect",
            Self::Borrow => "central.borrow",
            Self::Transfer => "central.transfer",
            Self::Release => "central.release",
            Self::Disconnect => "central.disconnect",
            Self::PeerLoss => "central.peer-loss",
            Self::AdapterReset => "central.adapter-reset",
            Self::Discover => "central.discover",
            Self::ServicesChanged => "central.services-changed",
            Self::Read => "central.read",
            Self::Write => "central.write",
            Self::LongWrite => "central.long-write",
            Self::ReadDescriptor => "central.read-descriptor",
            Self::WriteDescriptor => "central.write-descriptor",
            Self::SubscribeEnable => "central.subscribe-enable",
            Self::SubscribeDisable => "central.subscribe-disable",
            Self::Control => "central.control",
            Self::SecurityPair => "central.security-pair",
            Self::SecurityCancelPairing => "central.security-cancel-pairing",
            Self::SecurityUnpair => "central.security-unpair",
            Self::Destroy => "central.destroy",
        }
    }
}

/// One typed observation effect staged by a central operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CentralEffect {
    kind: CentralEffectKind,
    operation_id: OperationId,
    detail: String,
}

impl CentralEffect {
    /// Effect kind.
    #[must_use]
    pub const fn kind(&self) -> CentralEffectKind {
        self.kind
    }

    /// Owning operation id.
    #[must_use]
    pub const fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Human-readable detail (never a secret or raw payload).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// How one kernel operation settled through the central layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// Exactly one terminal outcome plus arbitration facts.
    Settled {
        kind: OperationTerminalKind,
        cause: Option<BleErrorCode>,
        reached_radio: bool,
        commit: crate::contracts::CommitState,
        suppressed: u64,
    },
    /// Input for an already-terminal operation: suppressed, state unchanged.
    DuplicateSuppressed { suppressed: u64 },
    /// An invalid contender on a live operation: ignored, state unchanged.
    ContenderIgnored,
}

/// Progress of one incremental destroy step (F15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestroyProgress {
    /// Queued ops settled as destroyed by this step.
    pub settled_queued: usize,
    /// True when the batch filled before all queued work settled; the host
    /// drains the batch and calls again.
    pub truncated: bool,
    /// Live (queued or dispatched) ops still owned.
    pub live_operations: usize,
    /// Terminal ops awaiting host `report_release_*`.
    pub terminal_pending_release: usize,
    /// True when no live work, no terminal pending, and no truncation: the
    /// host may take the final record via `destroy_record`.
    pub done: bool,
}

/// Path selector. Occurrence selects among duplicate UUIDs; a selector that
/// omits an occurrence while several candidates share the UUIDs fails with
/// `gatt.ambiguous-path` instead of guessing (GATT-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSelector {
    /// Canonical service UUID.
    pub service_uuid: String,
    /// Required service occurrence (`None` = UUID-only, may be ambiguous).
    pub service_occurrence: Option<u64>,
    /// Characteristic UUID (`None` = service-level path).
    pub characteristic_uuid: Option<String>,
    /// Required characteristic occurrence when a UUID is given.
    pub characteristic_occurrence: Option<u64>,
    /// Descriptor UUID (`None` = no descriptor level).
    pub descriptor_uuid: Option<String>,
    /// Required descriptor occurrence when a UUID is given.
    pub descriptor_occurrence: Option<u64>,
}

/// Consumer lifecycle within one subscription hub. The physical transitions
/// (`enabling`, `ready`, `removing`, `removed`, `failed`, `invalid`) follow
/// the frozen `subscription` table; per-consumer states reuse the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsumerState {
    Enabling,
    Ready,
    Removing,
    Removed,
    Failed,
    Invalid,
}

impl ConsumerState {
    /// Frozen wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabling => "enabling",
            Self::Ready => "ready",
            Self::Removing => "removing",
            Self::Removed => "removed",
            Self::Failed => "failed",
            Self::Invalid => "invalid",
        }
    }

    /// Whether this state is terminal (`removed`, `failed`, `invalid`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Removed | Self::Failed | Self::Invalid)
    }
}

/// Physical CCCD enablement owned by exactly one UBM owner per path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CccdPhysical {
    Disabled,
    Enabling,
    Enabled,
    Disabling,
    Failed,
    Invalid,
}

/// Single terminal overflow event for one consumer. Under the `error` policy
/// the rejected item is counted once (`dropped-items 1`,
/// `dropped-bytes <incoming>`, `replaced-items 0`); the terminal is emitted
/// exactly once and ingress closes afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionTerminal {
    dropped_items: u64,
    dropped_bytes: u64,
    replaced_items: u64,
}

impl SubscriptionTerminal {
    /// Terminal reason, always `overflow`.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        "overflow"
    }

    /// Items counted as dropped (always 1: the rejected item).
    #[must_use]
    pub const fn dropped_items(&self) -> u64 {
        self.dropped_items
    }

    /// Bytes counted as dropped (the rejected item's length).
    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Items replaced (always 0 under `error`: nothing is replaced).
    #[must_use]
    pub const fn replaced_items(&self) -> u64 {
        self.replaced_items
    }
}

/// Per-value delivery outcome for one consumer (GATT-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryOutcome {
    /// Value entered the consumer's bounded stream.
    Delivered,
    /// Stream applied a lossy policy and stayed active; the value is held
    /// with a coalescible overflow notice.
    OverflowNoticed,
    /// Value arrived before the consumer was ready: quarantined, never
    /// delivered.
    QuarantinedPreReady,
    /// Consumer was already removed: no delivery.
    DroppedRemoved,
    /// Stream terminated, hub invalid, or unknown consumer: no delivery.
    DroppedLate,
}

/// Stored GATT occurrence path with construction invariants enforced at
/// registration (pairing, scope) and generation copies for staleness checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPath {
    peer_key: String,
    attachment: AttachmentTuple,
    connection_generation: String,
    database_generation: String,
    service_uuid: String,
    service_occurrence: u64,
    characteristic_uuid: Option<String>,
    characteristic_occurrence: Option<u64>,
    descriptor_uuid: Option<String>,
    descriptor_occurrence: Option<u64>,
    properties: u8,
    owner_lease: String,
}

impl StoredPath {
    /// Canonical service UUID.
    #[must_use]
    pub fn service_uuid(&self) -> &str {
        &self.service_uuid
    }

    /// Service occurrence among duplicate UUIDs.
    #[must_use]
    pub const fn service_occurrence(&self) -> u64 {
        self.service_occurrence
    }

    /// Characteristic UUID, if this is a characteristic-level path.
    #[must_use]
    pub fn characteristic_uuid(&self) -> Option<&str> {
        self.characteristic_uuid.as_deref()
    }

    /// Characteristic occurrence among duplicate UUIDs, if this is a
    /// characteristic-level path.
    #[must_use]
    pub const fn characteristic_occurrence(&self) -> Option<u64> {
        self.characteristic_occurrence
    }

    /// Descriptor UUID, if this is a descriptor-level path.
    #[must_use]
    pub fn descriptor_uuid(&self) -> Option<&str> {
        self.descriptor_uuid.as_deref()
    }

    /// Descriptor occurrence among duplicate UUIDs, if this is a
    /// descriptor-level path.
    #[must_use]
    pub const fn descriptor_occurrence(&self) -> Option<u64> {
        self.descriptor_occurrence
    }

    /// Connection generation this path was discovered under.
    #[must_use]
    pub fn connection_generation(&self) -> &str {
        &self.connection_generation
    }

    /// Database generation this path was discovered under.
    #[must_use]
    pub fn database_generation(&self) -> &str {
        &self.database_generation
    }

    /// Owner lease.
    #[must_use]
    pub fn owner_lease(&self) -> &str {
        &self.owner_lease
    }

    /// Property flags.
    #[must_use]
    pub const fn properties(&self) -> u8 {
        self.properties
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScanSessionRecord {
    id: OperationId,
    state: ScanSessionState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerRecord {
    session_key: String,
    identity: PeerIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionRecord {
    peer_key: String,
    state: ConnectionState,
    connection_generation: Generation,
    database_generation: Generation,
    db_state: DatabaseState,
    leases: Vec<String>,
    sharing: bool,
    /// Operation owning the pending establishment, if the link never came up.
    connect_op: Option<OperationId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsumerRecord {
    lease: String,
    state: ConsumerState,
    op: Option<OperationId>,
    stream: Stream,
    terminal: Option<SubscriptionTerminal>,
    terminal_taken: bool,
    quarantined: u64,
    delivered: u64,
    /// One slot per stream-ledger entry, in arrival order (M2 value
    /// delivery): `Some` carries a value-carrying delivery's bytes, `None`
    /// marks a length-only delivery. Payload retention and stream
    /// accounting mutate together on every decision, so takes always pair
    /// a value with its own ledger size; popping frees stream bytes so the
    /// bound recycles.
    slots: VecDeque<Option<Vec<u8>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubscriptionHub {
    path_index: usize,
    physical: CccdPhysical,
    enable_op: Option<OperationId>,
    disable_op: Option<OperationId>,
    consumers: Vec<ConsumerRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PairState {
    Pairing,
    Paired,
    Cancelled,
    Unpaired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SecurityExchange {
    peer_key: String,
    state: PairState,
    op: Option<OperationId>,
}

/// Race/bounds/cleanup vector ids mirrored from the deterministic TCK seam.
/// Each id names behaviors this module serves on the real kernel.
pub const RACE_BOUNDS_CLEANUP_VECTOR_IDS: [&str; 7] = [
    "cleanup.failed-cleanup-retains-and-reports",
    "cleanup.duplicate-destroy-is-idempotent",
    "generation.stale-path-rejects-before-dispatch",
    "completion.duplicate-completion-settles-once",
    "bounds.subscription-overflow-is-bounded-and-terminal",
    "cancel.admission-completion-boundary-settles-once",
    "invalidation.service-change-invalidates-generation",
];

/// A reference-vs-contract disagreement resolved to the actual contract,
/// logged as an approved-correction candidate (never silent equality).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CorrectionCandidate {
    /// TCK vector or scenario the disagreement touches.
    pub vector_id: &'static str,
    /// What the reference seam appears to do.
    pub observed: &'static str,
    /// Contract resolution this module implements.
    pub resolution: &'static str,
}

/// Disagreements found while cross-checking the TCK seam's expectations,
/// each resolved to the frozen contract.
pub const APPROVED_CORRECTION_CANDIDATES: [CorrectionCandidate; 3] = [
    CorrectionCandidate {
        vector_id: "bounds.subscription-overflow-is-bounded-and-terminal",
        observed: "seam subscribes with byte budgets 8 and 4 while the reserved control budget is 64",
        resolution: "contract validateStreamLimits requires byteCapacity above reservedControlBytes; subscribe fails closed with stream.quota and overflow-terminal tests use contract-valid budgets",
    },
    CorrectionCandidate {
        vector_id: "bounds.subscription-overflow-is-bounded-and-terminal",
        observed: "seam terminal counts droppedItems 1 and droppedBytes of the rejected item while error-policy accounting keeps no drop counters",
        resolution: "contract error policy closes ingress with one terminal overflow; central counts the single rejected item on the terminal event and keeps accounting verbatim",
    },
    CorrectionCandidate {
        vector_id: "completion.duplicate-completion-settles-once",
        observed: "early draft rejected a late duplicate completion instead of observing it",
        resolution: "contract settles exactly once and records late duplicates as suppressed counts; duplicate completions suppress without a second settlement",
    },
];

/// Look up the correction candidate for one vector id, if any.
#[must_use]
pub const fn correction_for(vector_id: &str) -> Option<&'static CorrectionCandidate> {
    let mut index = 0;
    while index < APPROVED_CORRECTION_CANDIDATES.len() {
        let candidate = &APPROVED_CORRECTION_CANDIDATES[index];
        if string_eq(candidate.vector_id, vector_id) {
            return Some(candidate);
        }
        index += 1;
    }
    None
}

/// One transition owner for central/GATT work: scoped identities, connection
/// leases, occurrence paths, bounded subscriptions, capability truth, and
/// retained cleanup, all scheduled through the single [`Kernel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Central {
    config: CentralConfig,
    kernel: Kernel,
    attachment: AttachmentTuple,
    kernel_generation: Generation,
    op_counter: u64,
    conn_gen_counter: u64,
    db_gen_counter: u64,
    authority: u64,
    scans: Vec<ScanSessionRecord>,
    peers: Vec<PeerRecord>,
    connections: Vec<ConnectionRecord>,
    paths: Vec<StoredPath>,
    op_paths: Vec<(OperationId, usize)>,
    op_peers: Vec<(OperationId, String)>,
    hubs: Vec<SubscriptionHub>,
    capabilities: Vec<CapabilityDescriptor>,
    security: Vec<SecurityExchange>,
    typed_effects: Vec<CentralEffect>,
    destroy_record: Option<CleanupRecord>,
    disconnect_failures: Vec<CleanupRecord>,
    sharing_supported: bool,
    security_available: bool,
    restoration_authority: bool,
    handshake_complete: bool,
    /// Every kernel operation this owner admitted, in admission order. The
    /// kernel reaps entries only on release reports, so this ledger mirrors
    /// kernel membership for reset settlement.
    op_ids: Vec<OperationId>,
}

fn outcome_from_receipt(
    receipt: &crate::ownership::SettlementReceipt,
    stale_path: bool,
) -> CompletionOutcome {
    CompletionOutcome::Settled {
        kind: receipt.kind(),
        cause: if stale_path {
            Some(BleErrorCode::GattStaleHandle)
        } else {
            receipt.cause()
        },
        reached_radio: receipt.reached_radio(),
        commit: receipt.commit_state(),
        suppressed: receipt.suppressed_duplicates(),
    }
}

/// Map a security operation name to its typed effect kind. Returns `None`
/// for unknown operations, which fail closed as `argument.invalid`.
fn security_effect_kind(operation: &str) -> Option<CentralEffectKind> {
    if string_eq(operation, "pair") {
        Some(CentralEffectKind::SecurityPair)
    } else if string_eq(operation, "cancel-pairing") {
        Some(CentralEffectKind::SecurityCancelPairing)
    } else if string_eq(operation, "unpair") {
        Some(CentralEffectKind::SecurityUnpair)
    } else {
        None
    }
}

impl Central {
    /// Build a central owner bound to one attachment scope and generation
    /// with a completed handshake (PKG-02).
    pub fn new(
        attachment: AttachmentTuple,
        generation: Generation,
        config: CentralConfig,
    ) -> Result<Self, CoreError> {
        Self::new_with_handshake(
            attachment,
            generation,
            config,
            HandshakeState { complete: true },
        )
    }

    /// Build with an explicit handshake state. Admission before the handshake
    /// completes fails closed with `lifecycle.invalid-state` (PKG-02/OPS-01).
    pub fn new_with_handshake(
        attachment: AttachmentTuple,
        generation: Generation,
        config: CentralConfig,
        handshake: HandshakeState,
    ) -> Result<Self, CoreError> {
        let kernel = Kernel::new(
            config.kernel,
            attachment.clone(),
            generation.clone(),
            handshake,
        );
        Ok(Self {
            config,
            kernel,
            attachment,
            kernel_generation: generation,
            op_counter: 0,
            conn_gen_counter: 0,
            db_gen_counter: 0,
            authority: 0,
            scans: Vec::new(),
            peers: Vec::new(),
            connections: Vec::new(),
            paths: Vec::new(),
            op_paths: Vec::new(),
            op_peers: Vec::new(),
            hubs: Vec::new(),
            capabilities: Vec::new(),
            security: Vec::new(),
            typed_effects: Vec::new(),
            destroy_record: None,
            disconnect_failures: Vec::new(),
            sharing_supported: false,
            security_available: false,
            restoration_authority: true,
            handshake_complete: handshake.complete,
            op_ids: Vec::new(),
        })
    }

    fn next_op_id(&mut self) -> Result<OperationId, CoreError> {
        let id = OperationId::new(format!("central-op-{}", self.op_counter))?;
        self.op_counter = self.op_counter.saturating_add(1);
        Ok(id)
    }

    fn next_ordinal(&mut self) -> u64 {
        let ordinal = self.authority;
        self.authority = self.authority.saturating_add(1);
        ordinal
    }

    fn mint_connection_generation(&mut self) -> Result<Generation, CoreError> {
        let count = self.conn_gen_counter;
        self.conn_gen_counter = self.conn_gen_counter.saturating_add(1);
        let mut text = String::from("cg-");
        append_u64(&mut text, count);
        Generation::new(text)
    }

    fn mint_database_generation(&mut self) -> Result<Generation, CoreError> {
        let count = self.db_gen_counter;
        self.db_gen_counter = self.db_gen_counter.saturating_add(1);
        let mut text = String::from("db-");
        append_u64(&mut text, count);
        Generation::new(text)
    }

    /// Fail closed before mutating when the observation ledger is full, so a
    /// rejected request changes nothing and stages nothing.
    fn check_effect_room(&self) -> Result<(), CoreError> {
        if self.typed_effects.len() >= self.config.typed_effect_cap {
            return Err(err(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "central.effects.full",
            ));
        }
        Ok(())
    }

    /// Stage one typed observation effect. The caller holds effect room.
    fn stage_effect(&mut self, kind: CentralEffectKind, id: &OperationId, detail: &str) {
        self.typed_effects.push(CentralEffect {
            kind,
            operation_id: id.clone(),
            detail: String::from(detail),
        });
    }

    /// Admit one kernel operation under `owner`. Validation (handshake,
    /// attachment, generation, bounds, timeout) runs inside the kernel before
    /// any state changes.
    fn admit_op(
        &mut self,
        owner: &str,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        let id = self.next_op_id()?;
        let lease = LeaseId::new(String::from(owner))?;
        let generation = self.kernel_generation.clone();
        let attachment = self.attachment.clone();
        match self.kernel.handle(
            KernelInput::Admit {
                operation_id: id.clone(),
                owner: lease,
                attachment,
                generation,
                timeout_ms,
            },
            now,
            out,
        )? {
            HandleOutcome::Admitted { .. } => {
                self.op_ids.push(id.clone());
                Ok(id)
            }
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.admit.unexpected",
            )),
        }
    }

    fn scan_position(&self, id: &OperationId) -> Option<usize> {
        self.scans.iter().position(|scan| &scan.id == id)
    }

    fn peer_position(&self, session_key: &str) -> Option<usize> {
        self.peers
            .iter()
            .position(|peer| peer.session_key == session_key)
    }

    fn connection_position(&self, peer_key: &str) -> Option<usize> {
        self.connections
            .iter()
            .position(|connection| connection.peer_key == peer_key)
    }

    fn hub_position(&self, path_index: usize) -> Option<usize> {
        self.hubs
            .iter()
            .position(|hub| hub.path_index == path_index)
    }

    /// Whether a hub is garbage: physically invalid with no live operation
    /// and no untaken terminal anywhere in it (F04). Only such hubs
    /// reclaim; anything an operation or observation still references is
    /// kept, as is anything reusable (`Disabled`, `Failed`).
    fn hub_reclaimable(&self, hub: &SubscriptionHub) -> bool {
        if hub.physical != CccdPhysical::Invalid {
            return false;
        }
        hub.consumers.iter().all(|consumer| {
            let op_live = match &consumer.op {
                Some(op) => matches!(
                    self.kernel.operation_state(op),
                    Some(OpStateView::Queued) | Some(OpStateView::Dispatched)
                ),
                None => false,
            };
            let terminal_owed = consumer.terminal.is_some() && !consumer.terminal_taken;
            !op_live && !terminal_owed
        })
    }

    /// Drop reclaimable hubs so invalidated subscriptions never accumulate
    /// for the central's lifetime (F04). Runs at subscribe admission: after
    /// input validation, before hub lookup, so a dead hub neither blocks
    /// resubscribe nor consumes the hub bound.
    fn sweep_reclaimable_hubs(&mut self) {
        let mut dead: Vec<usize> = Vec::new();
        for (index, hub) in self.hubs.iter().enumerate() {
            if self.hub_reclaimable(hub) {
                dead.push(index);
            }
        }
        for index in dead.iter().rev() {
            self.hubs.remove(*index);
        }
    }

    fn security_position(&self, peer_key: &str) -> Option<usize> {
        self.security
            .iter()
            .position(|exchange| exchange.peer_key == peer_key)
    }

    /// Invalidate every subscription hub whose path belongs to `peer_key`.
    /// Used when a terminal record is replaced and on service change: after
    /// removal/revocation, later native values cannot reach old consumers.
    fn invalidate_peer_hubs(&mut self, peer_key: &str) {
        let mut stale_paths: Vec<usize> = Vec::new();
        for (index, path) in self.paths.iter().enumerate() {
            if path.peer_key == peer_key {
                stale_paths.push(index);
            }
        }
        for hub in self.hubs.iter_mut() {
            if stale_paths.contains(&hub.path_index) {
                hub.physical = CccdPhysical::Invalid;
                for consumer in hub.consumers.iter_mut() {
                    consumer.state = ConsumerState::Invalid;
                }
            }
        }
    }

    fn live_scan_active(&self) -> bool {
        self.scans.iter().any(|scan| {
            matches!(
                scan.state,
                ScanSessionState::Starting | ScanSessionState::Active | ScanSessionState::Stopping
            )
        })
    }

    /// Release the pending establishment claim held by `id`, if any. A
    /// cancelled or failed connect attempt frees its claim so a later client
    /// can connect; an established link is untouched.
    fn release_pending_connect_claim(&mut self, id: &OperationId) {
        let mut drop_record = false;
        for connection in self.connections.iter_mut() {
            let pending = match &connection.connect_op {
                Some(op) => op == id,
                None => false,
            };
            if !pending || connection.state != ConnectionState::Connecting {
                continue;
            }
            connection.connect_op = None;
            connection.leases.clear();
            drop_record = true;
            break;
        }
        if drop_record {
            self.connections.retain(|connection| {
                !(connection.state == ConnectionState::Connecting && connection.leases.is_empty())
            });
        }
        self.op_peers.retain(|(op, _)| op != id);
    }

    /// Borrow the attachment scope.
    #[must_use]
    pub fn attachment(&self) -> &AttachmentTuple {
        &self.attachment
    }

    /// Borrow the current kernel generation.
    #[must_use]
    pub fn kernel_generation(&self) -> &Generation {
        &self.kernel_generation
    }

    /// Number of live kernel operations.
    #[must_use]
    pub fn live_operation_count(&self) -> usize {
        self.kernel.live_operation_count()
    }

    /// Number of retained kernel cleanup records.
    #[must_use]
    pub fn retained_cleanup_count(&self) -> usize {
        self.kernel.retained_cleanup_count()
    }

    /// Observable lifecycle of one kernel operation, if present.
    #[must_use]
    pub fn operation_state(&self, id: &OperationId) -> Option<OpStateView> {
        self.kernel.operation_state(id)
    }

    /// Terminal operation ids awaiting host release (F02). The host reports
    /// the actual cleanup outcome per id via `report_release_success` or
    /// `report_release_failure`; this accessor never mutates.
    #[must_use]
    pub fn terminal_operation_ids(&self) -> Vec<OperationId> {
        self.op_ids
            .iter()
            .filter(|id| {
                matches!(
                    self.kernel.operation_state(id),
                    Some(OpStateView::Terminal(_))
                )
            })
            .cloned()
            .collect()
    }

    /// Live (queued or dispatched) operation ids (F03 testability): hosts use
    /// this to locate an in-flight op for cancellation races without guessing.
    #[must_use]
    pub fn live_operation_ids(&self) -> Vec<OperationId> {
        self.op_ids
            .iter()
            .filter(|id| {
                matches!(
                    self.kernel.operation_state(id),
                    Some(OpStateView::Queued) | Some(OpStateView::Dispatched)
                )
            })
            .cloned()
            .collect()
    }

    /// Late callbacks suppressed for one operation, if present.
    #[must_use]
    pub fn suppressed_count(&self, id: &OperationId) -> Option<u64> {
        self.kernel.suppressed_count(id)
    }

    /// Whether the backend reports shared-link support (OWN-01).
    pub fn set_sharing_supported(&mut self, sharing_supported: bool) {
        self.sharing_supported = sharing_supported;
    }

    /// Whether security operations may dispatch (SEC-02 fails closed).
    pub fn set_security_available(&mut self, available: bool) {
        self.security_available = available;
    }

    /// Whether a process-owned restoration authority survives (OWN-05).
    #[must_use]
    pub fn restoration_authority(&self) -> bool {
        self.restoration_authority
    }

    /// Drain staged typed observation effects, leaving the ledger empty.
    pub fn drain_typed_effects(&mut self) -> Vec<CentralEffect> {
        core::mem::take(&mut self.typed_effects)
    }

    /// Borrow staged typed observation effects without draining (F11): a
    /// host snapshots the length, runs one call, and inspects only the
    /// suffix that call staged — e.g. whether this `subscribe` issued the
    /// physical enable — leaving other calls' observations untouched.
    #[must_use]
    pub fn typed_effects(&self) -> &[CentralEffect] {
        &self.typed_effects
    }

    /// Start a scan: validate, arbitrate the one physical controller, and
    /// admit the scan lifecycle operation. A rejected second request changes
    /// nothing and stages nothing.
    pub fn start_scan(
        &mut self,
        request: &ScanRequest,
        share_token: Option<&str>,
        owner: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        if owner.is_empty() {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "scan.owner",
            ));
        }
        match arbitrate_scan_request(self.live_scan_active(), share_token) {
            OwnershipDecision::GrantPhysical | OwnershipDecision::GrantLease { .. } => {}
            OwnershipDecision::Reject { code } => {
                return Err(err(code, BleErrorDomain::Core, "scan.arbitration"));
            }
        }
        let id = self.admit_op(owner, request.timeout_ms(), now, out)?;
        self.scans.push(ScanSessionRecord {
            id: id.clone(),
            state: ScanSessionState::Starting,
        });
        self.stage_effect(CentralEffectKind::ScanStart, &id, "scan.start");
        Ok(id)
    }

    /// Settle the scan platform start (`platform-started`).
    pub fn platform_scan_started(&mut self, id: &OperationId) -> Result<(), CoreError> {
        self.check_effect_room()?;
        let index = self.scan_position(id).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "scan.unknown",
            )
        })?;
        let next = step_scan_session(self.scans[index].state, ScanPlatformEvent::PlatformStarted)?;
        self.scans[index].state = next;
        self.stage_effect(CentralEffectKind::ScanSettled, id, "scan.platform-started");
        Ok(())
    }

    /// Request scan stop (`stop`); the platform confirms via
    /// [`Central::note_scan_platform`].
    pub fn stop_scan(
        &mut self,
        id: &OperationId,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        let _ = (now, out);
        self.check_effect_room()?;
        let index = self.scan_position(id).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "scan.unknown",
            )
        })?;
        let next = step_scan_session(self.scans[index].state, ScanPlatformEvent::Stop)?;
        self.scans[index].state = next;
        self.stage_effect(CentralEffectKind::ScanSettled, id, "scan.stop");
        Ok(())
    }

    /// Feed one platform event into a scan session.
    pub fn note_scan_platform(
        &mut self,
        id: &OperationId,
        event: ScanPlatformEvent,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<ScanSessionState, CoreError> {
        self.check_effect_room()?;
        let index = self.scan_position(id).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "scan.unknown",
            )
        })?;
        let next = step_scan_session(self.scans[index].state, event)?;
        let ordinal = self.next_ordinal();
        let generation = self.kernel_generation.clone();
        let kind = match next {
            ScanSessionState::Stopped => ContenderKind::Success,
            ScanSessionState::Failed => ContenderKind::Failure,
            _ => ContenderKind::SessionStop,
        };
        let contender = Contender {
            ingress_ordinal: ordinal,
            kind,
            valid: true,
        };
        if next.is_terminal() {
            self.dispatch_before_success(id, kind, out)?;
            self.kernel.handle(
                KernelInput::Complete {
                    operation_id: id.clone(),
                    generation,
                    contender,
                },
                now,
                out,
            )?;
        }
        self.scans[index].state = next;
        self.stage_effect(CentralEffectKind::ScanSettled, id, "scan.platform-event");
        Ok(next)
    }

    /// Observe one scan session's state, if present.
    #[must_use]
    pub fn scan_session_state(&self, id: &OperationId) -> Option<ScanSessionState> {
        self.scan_position(id).map(|index| self.scans[index].state)
    }

    /// Whether a cached discovery may evict under pressure: no connection
    /// record, no operation naming it, and no security exchange reference
    /// it. Connected and selected peers pin; everything else rotates.
    fn peer_evictable(&self, session_key: &str) -> bool {
        self.connection_position(session_key).is_none()
            && !self.op_peers.iter().any(|(_, peer)| peer == session_key)
            && !self
                .security
                .iter()
                .any(|exchange| exchange.peer_key == session_key)
    }

    /// Resolve and register a peer identity; returns the session-scoped key
    /// (`domain:value`). Each unlinked identity stays distinct; address-like
    /// values are never treated as globally stable beyond their domain.
    /// The discovery cache is bounded independently of the connection
    /// bound: under pressure the oldest unreferenced discovery evicts
    /// (F16), while connected/selected peers pin. A cache full of pinned
    /// peers fails closed with an explicit quota.
    pub fn resolve_peer(&mut self, domain: &str, value: &str) -> Result<String, CoreError> {
        let Some(parsed) = PeerIdentityDomain::from_str(domain) else {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "peer-identity.domain",
            ));
        };
        let identity = PeerIdentity::new(self.attachment.clone(), parsed, String::from(value))?;
        let key = identity.session_key();
        if self.peer_position(&key).is_some() {
            return Ok(key);
        }
        if self.peers.len() >= self.config.max_discovered_peers {
            let victim = self
                .peers
                .iter()
                .position(|peer| self.peer_evictable(&peer.session_key));
            match victim {
                Some(index) => {
                    self.peers.remove(index);
                }
                None => {
                    return Err(err(
                        BleErrorCode::StreamQuota,
                        BleErrorDomain::Stream,
                        "peer.bound",
                    ));
                }
            }
        }
        self.peers.push(PeerRecord {
            session_key: key.clone(),
            identity,
        });
        Ok(key)
    }

    /// Number of registered logical peers (no duplicates after OWN-04).
    #[must_use]
    pub fn known_peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Require a registered peer, else `peer.not-found`.
    pub fn require_known_peer(&self, session_key: &str) -> Result<(), CoreError> {
        if self.peer_position(session_key).is_none() {
            return Err(err(
                BleErrorCode::PeerNotFound,
                BleErrorDomain::Connection,
                "peer.known",
            ));
        }
        Ok(())
    }

    /// Canonical identity changed after discovery (OWN-04): the native
    /// reconnect reference survives, no duplicate logical device appears.
    /// Returns the new session key.
    pub fn update_peer_canonical(
        &mut self,
        old_key: &str,
        new_domain: &str,
        new_value: &str,
    ) -> Result<String, CoreError> {
        let index = self.peer_position(old_key).ok_or_else(|| {
            err(
                BleErrorCode::PeerNotFound,
                BleErrorDomain::Connection,
                "peer.known",
            )
        })?;
        let Some(parsed) = PeerIdentityDomain::from_str(new_domain) else {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "peer-identity.domain",
            ));
        };
        let identity = PeerIdentity::new(self.attachment.clone(), parsed, String::from(new_value))?;
        let new_key = identity.session_key();
        if new_key != old_key && self.peer_position(&new_key).is_some() {
            // Collision: two cached identities resolve to one survivor.
            // Validate the merge BEFORE mutating (transactional): any veto
            // leaves every record untouched.
            let old_connection = self.connection_position(old_key);
            let new_connection = self.connection_position(&new_key);
            let mut drop_connection: Option<usize> = None;
            if let (Some(old_index), Some(new_index)) = (old_connection, new_connection) {
                let old_live = !self.connections[old_index].state.is_terminal();
                let new_live = !self.connections[new_index].state.is_terminal();
                if old_live && new_live {
                    // Two live links cannot merge: no single survivor owns
                    // both physical claims.
                    return Err(err(
                        BleErrorCode::OwnershipDenied,
                        BleErrorDomain::Core,
                        "peer.rekey-collision",
                    ));
                }
                // At most one side is live: drop a terminal record so the
                // survivor holds at most one link.
                if old_live {
                    drop_connection = Some(new_index);
                } else {
                    drop_connection = Some(old_index);
                }
            }
            let old_security = self.security_position(old_key);
            let new_security = self.security_position(&new_key);
            if let (Some(old_index), Some(_)) = (old_security, new_security)
                && self.security[old_index].state == PairState::Pairing
            {
                // The dropped exchange owns a live pairing op: merging
                // would strand its settlement.
                return Err(err(
                    BleErrorCode::OwnershipDenied,
                    BleErrorDomain::Core,
                    "peer.rekey-collision",
                ));
            }
            // Apply: every step below is infallible, so the collision
            // migrates connections, operations, paths, and security
            // together or not at all.
            if let Some(drop) = drop_connection {
                self.connections.remove(drop);
            }
            for connection in self.connections.iter_mut() {
                if connection.peer_key == old_key {
                    connection.peer_key = new_key.clone();
                }
            }
            for (_, peer) in self.op_peers.iter_mut() {
                if peer == old_key {
                    *peer = new_key.clone();
                }
            }
            for path in self.paths.iter_mut() {
                if path.peer_key == old_key {
                    path.peer_key = new_key.clone();
                }
            }
            Self::merge_security_on_rekey(&mut self.security, old_key, &new_key);
            self.peers.remove(index);
            return Ok(new_key);
        }
        self.peers[index].session_key = new_key.clone();
        self.peers[index].identity = identity;
        for connection in self.connections.iter_mut() {
            if connection.peer_key == old_key {
                connection.peer_key = new_key.clone();
            }
        }
        for (_, peer) in self.op_peers.iter_mut() {
            if peer == old_key {
                *peer = new_key.clone();
            }
        }
        for path in self.paths.iter_mut() {
            if path.peer_key == old_key {
                path.peer_key = new_key.clone();
            }
        }
        for exchange in self.security.iter_mut() {
            if exchange.peer_key == old_key {
                exchange.peer_key = new_key.clone();
            }
        }
        Ok(new_key)
    }

    /// Merge security exchanges on peer re-key collision: the surviving
    /// key keeps its exchange, the orphaned key's exchange is dropped.
    fn merge_security_on_rekey(security: &mut Vec<SecurityExchange>, old_key: &str, new_key: &str) {
        let old_position = security.iter().position(|known| known.peer_key == old_key);
        let Some(old_index) = old_position else {
            return;
        };
        if security.iter().any(|known| known.peer_key == new_key) {
            security.remove(old_index);
        } else {
            security[old_index].peer_key = String::from(new_key);
        }
    }

    /// Connect as the physical owner (first lease) or fail closed with
    /// `connection.already-owned` when the link is exclusively held (OWN-01).
    pub fn connect(
        &mut self,
        peer_key: &str,
        client_lease: &str,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        self.require_known_peer(peer_key)?;
        if client_lease.is_empty() {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "connection.lease",
            ));
        }
        if let Some(index) = self.connection_position(peer_key) {
            if !self.connections[index].state.is_terminal() {
                let leases = self.connections[index].leases.len() as u64;
                let sharing = self.connections[index].sharing;
                match arbitrate_connection_request(sharing, leases) {
                    OwnershipDecision::GrantPhysical => {}
                    OwnershipDecision::GrantLease { .. } => {
                        let id = self.admit_op(client_lease, timeout_ms, now, out)?;
                        self.connections[index]
                            .leases
                            .push(String::from(client_lease));
                        self.op_peers.push((id.clone(), String::from(peer_key)));
                        self.stage_effect(CentralEffectKind::Borrow, &id, "connection.borrow");
                        return Ok(id);
                    }
                    OwnershipDecision::Reject { code } => {
                        return Err(err(code, BleErrorDomain::Core, "connection.arbitration"));
                    }
                }
            } else {
                self.invalidate_peer_hubs(peer_key);
                self.connections.remove(index);
            }
        }
        if self.connections.len() >= self.config.max_connections {
            return Err(err(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "connection.bound",
            ));
        }
        let id = self.admit_op(client_lease, timeout_ms, now, out)?;
        let connection_generation = self.mint_connection_generation()?;
        let database_generation = self.mint_database_generation()?;
        self.connections.push(ConnectionRecord {
            peer_key: String::from(peer_key),
            state: ConnectionState::Connecting,
            connection_generation,
            database_generation,
            db_state: DatabaseState::Undiscovered,
            leases: Vec::from([String::from(client_lease)]),
            sharing: self.sharing_supported,
            connect_op: Some(id.clone()),
        });
        self.op_peers.push((id.clone(), String::from(peer_key)));
        self.stage_effect(CentralEffectKind::Connect, &id, "connection.connect");
        Ok(id)
    }

    /// Borrow the physical link under the explicit sharing rule. Without
    /// sharing support the borrow fails; nothing is shared merely because two
    /// clients name the same peer.
    pub fn borrow_connection(
        &mut self,
        peer_key: &str,
        client_lease: &str,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        self.require_known_peer(peer_key)?;
        if client_lease.is_empty() {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "connection.lease",
            ));
        }
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.borrow",
            )
        })?;
        if self.connections[index].state.is_terminal() {
            return Err(err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.borrow",
            ));
        }
        let leases = self.connections[index].leases.len() as u64;
        let sharing = self.connections[index].sharing;
        match arbitrate_connection_request(sharing, leases) {
            OwnershipDecision::GrantLease { .. } | OwnershipDecision::GrantPhysical => {}
            OwnershipDecision::Reject { code } => {
                return Err(err(code, BleErrorDomain::Core, "connection.arbitration"));
            }
        }
        let id = self.admit_op(client_lease, timeout_ms, now, out)?;
        self.connections[index]
            .leases
            .push(String::from(client_lease));
        self.op_peers.push((id.clone(), String::from(peer_key)));
        self.stage_effect(CentralEffectKind::Borrow, &id, "connection.borrow");
        Ok(id)
    }

    /// The link established (`link-established`).
    pub fn note_link_established(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.established",
            )
        })?;
        let next = step_connection(
            self.connections[index].state,
            ConnectionEvent::LinkEstablished,
        )?;
        self.connections[index].state = next;
        Ok(())
    }

    /// Transfer a lease between authenticated clients. Empty fields (source,
    /// destination, or generation) fail with `ownership.denied`; a stale
    /// generation fails with `connection.stale`. The epoch mirrors the
    /// frozen `ownership-transfer.epoch`: a `u64` is non-negative by type,
    /// and values above the JS safe-integer ceiling fail with
    /// `ownership.denied`.
    pub fn transfer_lease(
        &mut self,
        peer_key: &str,
        source: &str,
        dest: &str,
        generation: &str,
        epoch: u64,
    ) -> Result<(), CoreError> {
        const MAX_SAFE_EPOCH: u64 = 9_007_199_254_740_991;
        if source.is_empty() || dest.is_empty() || generation.is_empty() {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "lease.transfer",
            ));
        }
        if epoch > MAX_SAFE_EPOCH {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "lease.transfer",
            ));
        }
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "lease.transfer",
            )
        })?;
        if self.connections[index].connection_generation.as_str() != generation {
            return Err(err(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                "lease.transfer",
            ));
        }
        let held = self.connections[index]
            .leases
            .iter()
            .any(|lease| lease == source);
        if !held {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "lease.transfer",
            ));
        }
        self.connections[index]
            .leases
            .retain(|lease| lease != source);
        if !self.connections[index]
            .leases
            .iter()
            .any(|lease| lease == dest)
        {
            self.connections[index].leases.push(String::from(dest));
        }
        Ok(())
    }

    /// Release one lease. Returns true when the final release ends the
    /// physical link. An explicit owner disconnect also ends the link with
    /// borrowers attached (OWN-01).
    pub fn release_lease(
        &mut self,
        peer_key: &str,
        lease: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<bool, CoreError> {
        let _ = (now, out);
        self.check_effect_room()?;
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.release",
            )
        })?;
        let held = self.connections[index]
            .leases
            .iter()
            .any(|held| held == lease);
        if !held {
            return Err(err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.release",
            ));
        }
        self.connections[index].leases.retain(|held| held != lease);
        if self.connections[index].leases.is_empty() {
            let state = self.connections[index].state;
            if !state.is_terminal() {
                let next = step_connection(state, ConnectionEvent::Disconnect)?;
                self.connections[index].state = next;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Explicit disconnect (`disconnect` + in-flight settlement as
    /// `disconnected` + `link-released`).
    pub fn disconnect(
        &mut self,
        peer_key: &str,
        lease: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        let _ = (now, out);
        self.check_effect_room()?;
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.disconnect",
            )
        })?;
        let held = self.connections[index]
            .leases
            .iter()
            .any(|held| held == lease);
        if !held {
            return Err(err(
                BleErrorCode::OwnershipDenied,
                BleErrorDomain::Core,
                "connection.disconnect",
            ));
        }
        let next = step_connection(self.connections[index].state, ConnectionEvent::Disconnect)?;
        self.connections[index].state = next;
        self.invalidate_peer_hubs(peer_key);
        Ok(())
    }

    /// Link loss races explicit disconnect (CLN-02): exactly one terminal
    /// result, no double release, no resurrection. Returns the terminal
    /// state.
    pub fn note_peer_loss(
        &mut self,
        peer_key: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<ConnectionState, CoreError> {
        let _ = (now, out);
        self.check_effect_room()?;
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.peer-loss",
            )
        })?;
        let next = step_connection(self.connections[index].state, ConnectionEvent::PeerLoss)?;
        self.connections[index].state = next;
        self.connections[index].db_state = DatabaseState::Invalid;
        self.invalidate_peer_hubs(peer_key);
        Ok(next)
    }

    /// The platform confirms link release after disconnect.
    pub fn note_link_released(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.link-released",
            )
        })?;
        let next = step_connection(self.connections[index].state, ConnectionEvent::LinkReleased)?;
        self.connections[index].state = next;
        self.invalidate_peer_hubs(peer_key);
        Ok(())
    }

    /// Explicit disconnect failed (CLN-01): ownership of the failed cleanup
    /// is retained and reported; later disposal must not bypass it.
    pub fn report_disconnect_failure(
        &mut self,
        peer_key: &str,
        code: BleErrorCode,
    ) -> Result<(), CoreError> {
        if self.connection_position(peer_key).is_none() {
            return Err(err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.disconnect-failure",
            ));
        }
        let failure = CleanupFailure::new(String::from("connection"), code)?;
        let record = CleanupRecord::new(None, CleanupState::ReleaseFailed, Vec::from([failure]))?;
        self.disconnect_failures.push(record);
        Ok(())
    }

    /// Observe one connection's state, if present.
    #[must_use]
    pub fn connection_state(&self, peer_key: &str) -> Option<ConnectionState> {
        self.connection_position(peer_key)
            .map(|index| self.connections[index].state)
    }

    /// Leases currently holding one link.
    #[must_use]
    pub fn connection_lease_count(&self, peer_key: &str) -> usize {
        self.connection_position(peer_key)
            .map(|index| self.connections[index].leases.len())
            .unwrap_or(0)
    }

    /// Current connection generation for one peer, if connected.
    #[must_use]
    pub fn connection_generation(&self, peer_key: &str) -> Option<String> {
        self.connection_position(peer_key)
            .map(|index| self.connections[index].connection_generation.to_string())
    }

    /// Current database generation for one peer, if discovered.
    #[must_use]
    pub fn database_generation(&self, peer_key: &str) -> Option<String> {
        self.connection_position(peer_key).and_then(|index| {
            let connection = &self.connections[index];
            match connection.db_state {
                DatabaseState::Current | DatabaseState::Changed => {
                    Some(connection.database_generation.to_string())
                }
                _ => None,
            }
        })
    }

    /// Adapter reset during live work (OWN-06): live operations reach
    /// truthful terminals, generations invalidate, and a fresh kernel scope
    /// starts. Returns the number of operations settled by the reset.
    pub fn handle_adapter_reset(
        &mut self,
        new_attachment: AttachmentTuple,
        new_generation: Generation,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<usize, CoreError> {
        self.check_effect_room()?;
        let ordinal = self.next_ordinal();
        let generation = self.kernel_generation.clone();
        let mut settled = 0usize;
        let live: Vec<OperationId> = self.op_ids.clone();
        for id in live.iter() {
            let contender = Contender {
                ingress_ordinal: ordinal,
                kind: ContenderKind::Reset,
                valid: true,
            };
            let settled_one = self.kernel.handle(
                KernelInput::Complete {
                    operation_id: id.clone(),
                    generation: generation.clone(),
                    contender,
                },
                now,
                out,
            );
            match settled_one {
                Ok(HandleOutcome::Settled { .. }) => {
                    settled += 1;
                }
                Ok(HandleOutcome::ContenderIgnored) | Ok(HandleOutcome::DuplicateSuppressed) => {}
                Ok(_) => {
                    return Err(err(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "central.reset.settle",
                    ));
                }
                Err(error) if error.code() == BleErrorCode::ArgumentInvalid => {}
                Err(error) => return Err(error),
            }
        }
        let config_kernel = self.config.kernel;
        let handshake = HandshakeState {
            complete: self.handshake_complete,
        };
        self.kernel = Kernel::new(
            config_kernel,
            new_attachment.clone(),
            new_generation.clone(),
            handshake,
        );
        self.attachment = new_attachment;
        self.kernel_generation = new_generation;
        for scan in self.scans.iter_mut() {
            if !scan.state.is_terminal() {
                scan.state = ScanSessionState::Failed;
            }
        }
        self.connections.clear();
        self.op_ids.clear();
        self.op_paths.clear();
        self.op_peers.clear();
        for hub in self.hubs.iter_mut() {
            hub.physical = CccdPhysical::Invalid;
            for consumer in hub.consumers.iter_mut() {
                consumer.state = ConsumerState::Invalid;
            }
        }
        Ok(settled)
    }

    /// Begin discovery (`discover` / `rediscover`).
    pub fn begin_discovery(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.begin",
            )
        })?;
        if self.connections[index].state != ConnectionState::Connected {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "discovery.link",
            ));
        }
        let event = match self.connections[index].db_state {
            DatabaseState::Undiscovered => DatabaseEvent::Discover,
            DatabaseState::Current => DatabaseEvent::Rediscover,
            _ => {
                return Err(err(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "discovery.state",
                ));
            }
        };
        let next = step_database(self.connections[index].db_state, event)?;
        self.connections[index].db_state = next;
        Ok(())
    }

    /// Discovery snapshot complete (`snapshot-complete`).
    pub fn complete_discovery(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.complete",
            )
        })?;
        let next = step_database(
            self.connections[index].db_state,
            DatabaseEvent::SnapshotComplete,
        )?;
        let generation = self.mint_database_generation()?;
        self.connections[index].db_state = next;
        self.connections[index].database_generation = generation;
        Ok(())
    }

    /// Discovery failed (`discovery-failed`).
    pub fn fail_discovery(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.fail",
            )
        })?;
        let next = step_database(
            self.connections[index].db_state,
            DatabaseEvent::DiscoveryFailed,
        )?;
        self.connections[index].db_state = next;
        Ok(())
    }

    /// Service change arrived (GATT-02): the database moves to `changed`
    /// under a fresh generation; old handles can no longer operate.
    pub fn services_changed(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.services-changed",
            )
        })?;
        let next = step_database(
            self.connections[index].db_state,
            DatabaseEvent::ServicesChanged,
        )?;
        let generation = self.mint_database_generation()?;
        self.connections[index].db_state = next;
        self.connections[index].database_generation = generation;
        self.invalidate_peer_hubs(peer_key);
        Ok(())
    }

    /// Deliberate rediscovery required before new handles (`require-rediscovery`).
    pub fn require_rediscovery(&mut self, peer_key: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.rediscovery",
            )
        })?;
        let next = step_database(
            self.connections[index].db_state,
            DatabaseEvent::RequireRediscovery,
        )?;
        self.connections[index].db_state = next;
        Ok(())
    }

    /// Observe one database's state, if present.
    #[must_use]
    pub fn database_state(&self, peer_key: &str) -> Option<DatabaseState> {
        self.connection_position(peer_key)
            .map(|index| self.connections[index].db_state)
    }

    /// Read the current snapshot. A `changed` database fails with
    /// `gatt.stale-handle` (rediscover first); anything else off-`current`
    /// fails with `gatt.discovery-required`. Returns the live path count:
    /// only paths under the peer's current generations (F04), never
    /// historical entries.
    pub fn snapshot_path_count(&self, peer_key: &str) -> Result<usize, CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.snapshot",
            )
        })?;
        match self.connections[index].db_state {
            DatabaseState::Current => {
                let connection = &self.connections[index];
                Ok(self
                    .paths
                    .iter()
                    .filter(|path| {
                        path.peer_key == peer_key
                            && path.attachment == self.attachment
                            && path.connection_generation
                                == connection.connection_generation.as_str()
                            && path.database_generation == connection.database_generation.as_str()
                    })
                    .count())
            }
            DatabaseState::Changed => Err(err(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "discovery.snapshot",
            )),
            _ => Err(err(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "discovery.snapshot",
            )),
        }
    }

    /// Read the current snapshot paths in registration order (F01: the
    /// dispatch surface renders databases and builds selectors from this,
    /// not from counts). Same guards as [`Self::snapshot_path_count`]: a
    /// `changed` database fails with `gatt.stale-handle`, anything else
    /// off-`current` with `gatt.discovery-required`. Only paths under the
    /// peer's current generations are returned (F04), never historical
    /// entries.
    pub fn snapshot_paths(&self, peer_key: &str) -> Result<Vec<StoredPath>, CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "discovery.snapshot",
            )
        })?;
        match self.connections[index].db_state {
            DatabaseState::Current => {
                let connection = &self.connections[index];
                Ok(self
                    .paths
                    .iter()
                    .filter(|path| {
                        path.peer_key == peer_key
                            && path.attachment == self.attachment
                            && path.connection_generation
                                == connection.connection_generation.as_str()
                            && path.database_generation == connection.database_generation.as_str()
                    })
                    .cloned()
                    .collect())
            }
            DatabaseState::Changed => Err(err(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "discovery.snapshot",
            )),
            _ => Err(err(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "discovery.snapshot",
            )),
        }
    }

    /// Live (fresh-handle) paths across all peers. Stale generations are
    /// history, not capacity: the path bound counts this, never the raw
    /// table length (F04).
    fn live_path_count(&self) -> usize {
        self.paths
            .iter()
            .enumerate()
            .filter(|(slot, _)| self.check_path_fresh(*slot).is_ok())
            .count()
    }

    /// Register one discovered occurrence path. Pairing and scope invariants
    /// are enforced here (UUID-only selection stays prohibited downstream).
    /// Re-registering an identical selector revives its stale slot when no
    /// unreleased operation references it (F04): same-table rediscovery
    /// reuses indices instead of growing history; raw indices are never
    /// recycled for a different selector.
    #[allow(clippy::too_many_arguments)]
    pub fn register_path(
        &mut self,
        peer_key: &str,
        service_uuid: &str,
        service_occurrence: u64,
        characteristic_uuid: Option<&str>,
        characteristic_occurrence: Option<u64>,
        descriptor_uuid: Option<&str>,
        descriptor_occurrence: Option<u64>,
        properties: u8,
        owner_lease: &str,
    ) -> Result<usize, CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "path.register",
            )
        })?;
        if self.connections[index].db_state != DatabaseState::Current {
            return Err(err(
                BleErrorCode::GattDiscoveryRequired,
                BleErrorDomain::Gatt,
                "path.register",
            ));
        }
        if characteristic_uuid.is_some() != characteristic_occurrence.is_some() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.characteristic-pair",
            ));
        }
        if descriptor_uuid.is_some() != descriptor_occurrence.is_some() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.descriptor-pair",
            ));
        }
        if descriptor_uuid.is_some() && characteristic_uuid.is_none() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.descriptor-scope",
            ));
        }
        if owner_lease.is_empty() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.owner",
            ));
        }
        if self.live_path_count() >= self.config.max_paths {
            return Err(err(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "path.bound",
            ));
        }
        let canonical_service = canonical_uuid(service_uuid)?;
        let canonical_characteristic = match characteristic_uuid {
            Some(uuid) => Some(canonical_uuid(uuid)?),
            None => None,
        };
        let canonical_descriptor = match descriptor_uuid {
            Some(uuid) => Some(canonical_uuid(uuid)?),
            None => None,
        };
        let connection_generation = self.connections[index].connection_generation.to_string();
        let database_generation = self.connections[index].database_generation.to_string();
        // Revive (F04): an identical selector in a stale slot reuses its
        // index when no unreleased operation references it, so same-table
        // rediscovery never grows history. Live identical slots still
        // append (existing duplicate contract); op-referenced stale slots
        // are left for a later revive to keep generation protection.
        let revive = self.paths.iter().enumerate().position(|(slot, path)| {
            path.peer_key == peer_key
                && path.service_uuid == canonical_service
                && path.service_occurrence == service_occurrence
                && path.characteristic_uuid == canonical_characteristic
                && path.characteristic_occurrence == characteristic_occurrence
                && path.descriptor_uuid == canonical_descriptor
                && path.descriptor_occurrence == descriptor_occurrence
                && self.check_path_fresh(slot).is_err()
                && !self.op_paths.iter().any(|(_, known)| *known == slot)
        });
        if let Some(slot) = revive {
            let stored = &mut self.paths[slot];
            stored.attachment = self.attachment.clone();
            stored.connection_generation = connection_generation;
            stored.database_generation = database_generation;
            stored.properties = properties;
            stored.owner_lease = String::from(owner_lease);
            return Ok(slot);
        }
        let stored = StoredPath {
            peer_key: String::from(peer_key),
            attachment: self.attachment.clone(),
            connection_generation,
            database_generation,
            service_uuid: canonical_service,
            service_occurrence,
            characteristic_uuid: canonical_characteristic,
            characteristic_occurrence,
            descriptor_uuid: canonical_descriptor,
            descriptor_occurrence,
            properties,
            owner_lease: String::from(owner_lease),
        };
        self.paths.push(stored);
        Ok(self.paths.len() - 1)
    }

    /// Resolve a selector to one stored path (GATT-01). UUID-only selection
    /// across duplicates fails with `gatt.ambiguous-path`; no match fails
    /// with `gatt.not-found`. New selectors resolve ONLY against the
    /// peer's current attachment/connection/database generation (F04):
    /// stale snapshots never match, so rediscovery cannot report
    /// ambiguity; old handles stay rejectable as stale via
    /// [`Central::check_path_fresh`].
    pub fn resolve_path(
        &self,
        peer_key: &str,
        selector: &PathSelector,
    ) -> Result<usize, CoreError> {
        let current = self
            .connection_position(peer_key)
            .and_then(|index| self.connections.get(index));
        let Some(connection) = current else {
            // No live snapshot: nothing is current for this peer.
            return Err(err(
                BleErrorCode::GattNotFound,
                BleErrorDomain::Gatt,
                "path.resolve",
            ));
        };
        let mut matches: Vec<usize> = Vec::new();
        for (index, path) in self.paths.iter().enumerate() {
            if path.peer_key != peer_key || path.service_uuid != selector.service_uuid {
                continue;
            }
            if path.attachment != self.attachment
                || path.connection_generation != connection.connection_generation.as_str()
                || path.database_generation != connection.database_generation.as_str()
            {
                continue;
            }
            if let Some(occurrence) = selector.service_occurrence
                && path.service_occurrence != occurrence
            {
                continue;
            }
            match (&selector.characteristic_uuid, &path.characteristic_uuid) {
                (None, None) => {}
                (Some(want), Some(have)) if want == have => {}
                _ => continue,
            }
            if let Some(occurrence) = selector.characteristic_occurrence
                && path.characteristic_occurrence != Some(occurrence)
            {
                continue;
            }
            match (&selector.descriptor_uuid, &path.descriptor_uuid) {
                (None, None) => {}
                (Some(want), Some(have)) if want == have => {}
                _ => continue,
            }
            if let Some(occurrence) = selector.descriptor_occurrence
                && path.descriptor_occurrence != Some(occurrence)
            {
                continue;
            }
            matches.push(index);
        }
        if matches.is_empty() {
            return Err(err(
                BleErrorCode::GattNotFound,
                BleErrorDomain::Gatt,
                "path.resolve",
            ));
        }
        if matches.len() > 1 {
            return Err(err(
                BleErrorCode::GattAmbiguousPath,
                BleErrorDomain::Gatt,
                "path.resolve",
            ));
        }
        Ok(matches[0])
    }

    /// Reject a foreign handle before any radio effect (`connection.stale`,
    /// OWN-02) and a stale generation (`gatt.stale-handle`, GATT-02).
    pub fn check_path_fresh(&self, path_index: usize) -> Result<(), CoreError> {
        let path = self.paths.get(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.index",
            )
        })?;
        assert_same_attachment(&path.attachment, &self.attachment, "path.scope")?;
        let connection = self
            .connection_position(&path.peer_key)
            .and_then(|index| self.connections.get(index))
            .ok_or_else(|| {
                err(
                    BleErrorCode::ConnectionStale,
                    BleErrorDomain::Connection,
                    "path.connection",
                )
            })?;
        if path.connection_generation != connection.connection_generation.as_str()
            || path.database_generation != connection.database_generation.as_str()
        {
            return Err(err(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "path.generation",
            ));
        }
        Ok(())
    }

    /// Borrow one stored path, if present.
    #[must_use]
    pub fn stored_path(&self, path_index: usize) -> Option<&StoredPath> {
        self.paths.get(path_index)
    }

    /// Start a read: freshness, discovery, lease, and property validation run
    /// before kernel admission, so a stale path never dispatches.
    pub fn start_read(
        &mut self,
        path_index: usize,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        self.check_path_fresh(path_index)?;
        let (owner, peer_key) = self.require_gatt_ready(path_index, GATT_PROP_READ, "read")?;
        let id = self.admit_op(&owner, timeout_ms, now, out)?;
        self.op_paths.push((id.clone(), path_index));
        self.op_peers.push((id.clone(), peer_key));
        self.stage_effect(CentralEffectKind::Read, &id, "gatt.read");
        Ok(id)
    }

    /// Shared GATT readiness gate: the link is live, the database is current,
    /// and the path carries the property the operation needs. Every check
    /// runs before kernel admission. Returns the owner lease and peer key.
    fn require_gatt_ready(
        &self,
        path_index: usize,
        required_property: u8,
        operation: &str,
    ) -> Result<(String, String), CoreError> {
        let (owner, peer_key) = self.require_gatt_link(path_index, operation)?;
        let path = self.paths.get(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.index",
            )
        })?;
        if path.properties & required_property == 0 {
            return Err(err(
                BleErrorCode::GattPropertyNotSupported,
                BleErrorDomain::Gatt,
                operation,
            ));
        }
        Ok((owner, peer_key))
    }

    /// Link, database, and lease gate without a property check. Subscribe
    /// needs notify-or-indicate rather than one fixed flag, so it gates here
    /// and checks its property separately.
    fn require_gatt_link(
        &self,
        path_index: usize,
        operation: &str,
    ) -> Result<(String, String), CoreError> {
        let path = self.paths.get(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.index",
            )
        })?;
        let index = self.connection_position(&path.peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                operation,
            )
        })?;
        let connection = &self.connections[index];
        if connection.state != ConnectionState::Connected {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                operation,
            ));
        }
        match connection.db_state {
            DatabaseState::Current => {}
            DatabaseState::Changed => {
                return Err(err(
                    BleErrorCode::GattStaleHandle,
                    BleErrorDomain::Gatt,
                    operation,
                ));
            }
            _ => {
                return Err(err(
                    BleErrorCode::GattDiscoveryRequired,
                    BleErrorDomain::Gatt,
                    operation,
                ));
            }
        }
        if connection.leases.is_empty() {
            return Err(err(
                BleErrorCode::ConnectionStale,
                BleErrorDomain::Connection,
                operation,
            ));
        }
        Ok((path.owner_lease.clone(), path.peer_key.clone()))
    }

    /// Start a write, mirroring C-UBM `validateWriteRequest` field order:
    /// mode, mode support, connection generation, database generation,
    /// maximum availability, then size.
    #[allow(clippy::too_many_arguments)]
    pub fn start_write(
        &mut self,
        path_index: usize,
        mode: &str,
        value_len: u64,
        effective_maximum: Option<u64>,
        mode_supported: bool,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        let Some(parsed) = WriteMode::from_str(mode) else {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Gatt,
                "write.mode",
            ));
        };
        if !mode_supported {
            return Err(err(
                BleErrorCode::CapabilityUnsupported,
                BleErrorDomain::Capability,
                "write.mode",
            ));
        }
        self.check_path_fresh(path_index)?;
        let (owner, peer_key) =
            self.require_gatt_ready(path_index, parsed.required_property(), "write")?;
        let Some(maximum) = effective_maximum else {
            return Err(err(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                "write.maximum",
            ));
        };
        if value_len > maximum {
            return Err(err(
                BleErrorCode::BytesTooLarge,
                BleErrorDomain::Gatt,
                "write.length",
            ));
        }
        let id = self.admit_op(&owner, timeout_ms, now, out)?;
        self.op_paths.push((id.clone(), path_index));
        self.op_peers.push((id.clone(), peer_key));
        self.stage_effect(CentralEffectKind::Write, &id, "gatt.write");
        Ok(id)
    }

    /// Start a descriptor read (descriptor-level paths only).
    pub fn start_read_descriptor(
        &mut self,
        path_index: usize,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        self.require_descriptor_path(path_index)?;
        self.check_path_fresh(path_index)?;
        let (owner, peer_key) = self.require_gatt_ready(path_index, GATT_PROP_READ, "read")?;
        let id = self.admit_op(&owner, timeout_ms, now, out)?;
        self.op_paths.push((id.clone(), path_index));
        self.op_peers.push((id.clone(), peer_key));
        self.stage_effect(
            CentralEffectKind::ReadDescriptor,
            &id,
            "gatt.read-descriptor",
        );
        Ok(id)
    }

    /// Start a descriptor write. Direct CCCD writes fail with
    /// `gatt.cccd-managed`: CCCD state moves only through subscribe.
    pub fn start_write_descriptor(
        &mut self,
        path_index: usize,
        value_len: u64,
        effective_maximum: Option<u64>,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        self.require_descriptor_path(path_index)?;
        self.check_path_fresh(path_index)?;
        if self.is_cccd_path(path_index) {
            return Err(err(
                BleErrorCode::GattCccdManaged,
                BleErrorDomain::Gatt,
                "write.cccd",
            ));
        }
        let (owner, peer_key) = self.require_gatt_ready(path_index, GATT_PROP_WRITE, "write")?;
        let Some(maximum) = effective_maximum else {
            return Err(err(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                "write.maximum",
            ));
        };
        if value_len > maximum {
            return Err(err(
                BleErrorCode::BytesTooLarge,
                BleErrorDomain::Gatt,
                "write.length",
            ));
        }
        let id = self.admit_op(&owner, timeout_ms, now, out)?;
        self.op_paths.push((id.clone(), path_index));
        self.op_peers.push((id.clone(), peer_key));
        self.stage_effect(
            CentralEffectKind::WriteDescriptor,
            &id,
            "gatt.write-descriptor",
        );
        Ok(id)
    }

    fn require_descriptor_path(&self, path_index: usize) -> Result<(), CoreError> {
        let path = self.paths.get(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.index",
            )
        })?;
        if path.descriptor_uuid.is_none() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "path.descriptor-level",
            ));
        }
        Ok(())
    }

    fn is_cccd_path(&self, path_index: usize) -> bool {
        match self.paths.get(path_index) {
            Some(path) => match &path.descriptor_uuid {
                Some(uuid) => uuid == CCCD_UUID,
                None => false,
            },
            None => false,
        }
    }

    /// Compose the effective maximum write length from measured maxima. An
    /// unmeasured maximum is `capability.unavailable`, never infinity.
    pub fn maximum_write_length(
        &self,
        operation_payload_limit: Option<u64>,
        negotiated_directional_limit: Option<u64>,
        backend_limit: Option<u64>,
        operation: &str,
    ) -> Result<u64, CoreError> {
        let mut measured: Vec<u64> = Vec::with_capacity(3);
        for limit in [
            operation_payload_limit,
            negotiated_directional_limit,
            backend_limit,
        ] {
            match limit {
                Some(bound) => measured.push(bound),
                None => {
                    return Err(err(
                        BleErrorCode::CapabilityUnavailable,
                        BleErrorDomain::Capability,
                        operation,
                    ));
                }
            }
        }
        effective_max_bytes(&measured)
    }

    /// Execute a long write as sequential kernel segments with partial-failure
    /// semantics: the first failing segment stops the sequence, completed
    /// segments stay completed, and the outcome reports the failure.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_long_write(
        &mut self,
        path_index: usize,
        value_len: u64,
        operation_payload_limit: Option<u64>,
        negotiated_directional_limit: Option<u64>,
        backend_limit: Option<u64>,
        fail_at_segment: Option<u64>,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<LongWriteOutcome, CoreError> {
        self.check_effect_room()?;
        self.check_path_fresh(path_index)?;
        let (owner, peer_key) =
            self.require_gatt_ready(path_index, GATT_PROP_WRITE, "long-write")?;
        let plan = plan_long_write(
            value_len,
            operation_payload_limit,
            negotiated_directional_limit,
            backend_limit,
        )?;
        let mut completed = 0u64;
        let mut failed_at: Option<u64> = None;
        let mut segment = 0u64;
        while segment < plan.segments() {
            let id = self.admit_op(&owner, timeout_ms, now, out)?;
            self.op_paths.push((id.clone(), path_index));
            self.op_peers.push((id.clone(), peer_key.clone()));
            self.stage_effect(CentralEffectKind::LongWrite, &id, "gatt.long-write");
            let generation = self.kernel_generation.clone();
            self.kernel.handle(
                KernelInput::Dispatch {
                    operation_id: id.clone(),
                    generation: generation.clone(),
                },
                now,
                out,
            )?;
            let fail_here = match fail_at_segment {
                Some(index) => index == segment,
                None => false,
            };
            let contender = Contender {
                ingress_ordinal: segment,
                kind: if fail_here {
                    ContenderKind::Failure
                } else {
                    ContenderKind::Success
                },
                valid: true,
            };
            match self.kernel.handle(
                KernelInput::Complete {
                    operation_id: id,
                    generation,
                    contender,
                },
                now,
                out,
            )? {
                HandleOutcome::Settled { receipt }
                    if receipt.kind() == OperationTerminalKind::Succeeded =>
                {
                    completed += 1;
                }
                _ => {
                    failed_at = Some(segment);
                    break;
                }
            }
            segment += 1;
        }
        Ok(LongWriteOutcome {
            segments: plan.segments(),
            completed,
            failed_at,
        })
    }

    /// Dispatch one admitted operation to the radio.
    pub fn dispatch_op(
        &mut self,
        id: &OperationId,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        let generation = self.kernel_generation.clone();
        // Dispatch carries no timing of its own; the kernel ignores the clock
        // for this input (deadlines were fixed at admission).
        self.kernel.handle(
            KernelInput::Dispatch {
                operation_id: id.clone(),
                generation,
            },
            0,
            out,
        )?;
        Ok(())
    }

    /// Dispatch a queued operation before a success settlement. The frozen
    /// operation machine reaches `succeeded` only via `dispatched→settling`,
    /// so the kernel rejects an undispatched success; other contenders
    /// settle from `queued` directly per the machine and pass through
    /// untouched.
    fn dispatch_before_success(
        &mut self,
        id: &OperationId,
        kind: ContenderKind,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        if !matches!(kind, ContenderKind::Success | ContenderKind::DispatchBegin) {
            return Ok(());
        }
        if self.kernel.operation_state(id) == Some(OpStateView::Queued) {
            self.dispatch_op(id, out)?;
        }
        Ok(())
    }

    /// Settle one operation with a contender. A path that went stale while
    /// dispatched settles truthfully and reports `gatt.stale-handle`; a
    /// duplicate completion suppresses without a second settlement.
    pub fn settle_op(
        &mut self,
        id: &OperationId,
        kind: ContenderKind,
        valid: bool,
        ordinal: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<CompletionOutcome, CoreError> {
        let stale_path = match self.op_paths.iter().find(|(op, _)| op == id) {
            Some((_, path_index)) => self.check_path_fresh(*path_index).is_err(),
            None => false,
        };
        let generation = self.kernel_generation.clone();
        let effective = if stale_path {
            ContenderKind::Failure
        } else {
            kind
        };
        self.dispatch_before_success(id, effective, out)?;
        let contender = Contender {
            ingress_ordinal: ordinal,
            kind: effective,
            valid,
        };
        let outcome = self.kernel.handle(
            KernelInput::Complete {
                operation_id: id.clone(),
                generation,
                contender,
            },
            now,
            out,
        )?;
        match outcome {
            HandleOutcome::Settled { receipt } => {
                let settled = outcome_from_receipt(&receipt, stale_path);
                if receipt.kind() != OperationTerminalKind::Succeeded {
                    self.release_pending_connect_claim(id);
                }
                Ok(settled)
            }
            HandleOutcome::DuplicateSuppressed => {
                let suppressed = self.kernel.suppressed_count(id).unwrap_or(0);
                Ok(CompletionOutcome::DuplicateSuppressed { suppressed })
            }
            HandleOutcome::ContenderIgnored => Ok(CompletionOutcome::ContenderIgnored),
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.settle.unexpected",
            )),
        }
    }

    /// Cancel one operation (OPS-01): a queued operation never reaches the
    /// radio and queue ownership releases with phase receipts. Cancellation
    /// also releases the operation's subscription wait: an `Enabling`
    /// consumer owned by the cancelled op leaves the wait set, so a late
    /// enable success can never revive it (F12).
    pub fn cancel_op(
        &mut self,
        id: &OperationId,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<CompletionOutcome, CoreError> {
        let generation = self.kernel_generation.clone();
        let outcome = self.kernel.handle(
            KernelInput::Cancel {
                operation_id: id.clone(),
                generation,
            },
            now,
            out,
        )?;
        match outcome {
            HandleOutcome::Cancelled { receipt } => {
                let settled = outcome_from_receipt(&receipt, false);
                self.release_pending_connect_claim(id);
                for hub in self.hubs.iter_mut() {
                    for consumer in hub.consumers.iter_mut() {
                        let owned = match &consumer.op {
                            Some(op) => op == id,
                            None => false,
                        };
                        if owned && consumer.state == ConsumerState::Enabling {
                            consumer.state = ConsumerState::Removed;
                        }
                    }
                }
                Ok(settled)
            }
            HandleOutcome::DuplicateSuppressed => {
                let suppressed = self.kernel.suppressed_count(id).unwrap_or(0);
                Ok(CompletionOutcome::DuplicateSuppressed { suppressed })
            }
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.cancel.unexpected",
            )),
        }
    }

    /// Settle queued operations whose deadline passed. Returns
    /// `(settled, truncated)`.
    pub fn expire_sweep(
        &mut self,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(usize, bool), CoreError> {
        match self.kernel.handle(KernelInput::ExpireSweep, now, out)? {
            HandleOutcome::Swept { settled, truncated } => {
                let expired: Vec<OperationId> = self
                    .op_ids
                    .iter()
                    .filter(|id| {
                        self.kernel.operation_state(id)
                            == Some(OpStateView::Terminal(OperationTerminalKind::TimedOut))
                    })
                    .cloned()
                    .collect();
                for id in expired.iter() {
                    self.release_pending_connect_claim(id);
                }
                Ok((settled, truncated))
            }
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.sweep.unexpected",
            )),
        }
    }

    /// Register a runtime capability descriptor.
    pub fn register_capability(
        &mut self,
        descriptor: CapabilityDescriptor,
    ) -> Result<(), CoreError> {
        match self
            .capabilities
            .iter()
            .position(|known| known.id() == descriptor.id())
        {
            Some(index) => {
                self.capabilities[index] = descriptor;
            }
            None => {
                self.capabilities.push(descriptor);
            }
        }
        Ok(())
    }

    /// Gate one operation on a capability. Unknown ids fail closed with
    /// `capability.unsupported`; `limited` proceeds with an explicit
    /// limitation admission; required rows are never erased.
    pub fn check_capability(
        &self,
        id: &str,
        operation: &str,
    ) -> Result<CapabilityAdmission, CoreError> {
        let _ = operation;
        match self.capabilities.iter().find(|known| known.id() == id) {
            Some(descriptor) => match descriptor.state() {
                CapabilityState::Supported => Ok(CapabilityAdmission::Proceed),
                CapabilityState::Limited => Ok(CapabilityAdmission::ProceedWithLimitation),
                CapabilityState::Unsupported => Err(err(
                    BleErrorCode::CapabilityUnsupported,
                    BleErrorDomain::Capability,
                    "capability.gate",
                )),
                CapabilityState::Unavailable => Err(err(
                    BleErrorCode::CapabilityUnavailable,
                    BleErrorDomain::Capability,
                    "capability.gate",
                )),
            },
            None => Err(err(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                "capability.unknown",
            )),
        }
    }

    /// Required parity rows, always complete: marking a capability
    /// unsupported changes its reported state, never its row.
    #[must_use]
    pub fn parity_rows(&self) -> Vec<(String, CapabilityState)> {
        let mut rows: Vec<(String, CapabilityState)> =
            Vec::with_capacity(REQUIRED_CAPABILITY_IDS.len());
        for id in REQUIRED_CAPABILITY_IDS.iter() {
            let state = match self.capabilities.iter().find(|known| known.id() == *id) {
                Some(descriptor) => descriptor.state(),
                None => CapabilityState::Unavailable,
            };
            rows.push((String::from(*id), state));
        }
        rows
    }

    /// Dispatch one required connection control as a typed effect. Returns
    /// the operation plus `proves_selection = false`: acceptance is dispatch
    /// acceptance, never proof the controller or peer selected the request.
    pub fn start_control(
        &mut self,
        method: &str,
        peer_key: Option<&str>,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(OperationId, bool), CoreError> {
        self.check_effect_room()?;
        let control = match central_control_for(method) {
            Some(control) => control,
            None => {
                return Err(err(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "control.method",
                ));
            }
        };
        if let Some(peer) = peer_key {
            self.require_known_peer(peer)?;
        }
        match self.check_capability(control.capability_id, "control")? {
            CapabilityAdmission::Proceed | CapabilityAdmission::ProceedWithLimitation => {}
        }
        let owner = match peer_key {
            Some(peer) => String::from(peer),
            None => String::from(method),
        };
        let id = self.admit_op(&owner, timeout_ms, now, out)?;
        if let Some(peer) = peer_key {
            self.op_peers.push((id.clone(), String::from(peer)));
        }
        self.stage_effect(CentralEffectKind::Control, &id, "control.request");
        Ok((id, false))
    }

    /// Start a security operation (`pair`, `cancel-pairing`, `unpair`) as a
    /// typed effect. Without security availability it fails closed with
    /// `platform.security` (SEC-02: no silent downgrade).
    pub fn start_security(
        &mut self,
        operation: &str,
        peer_key: &str,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        let kind = match security_effect_kind(operation) {
            Some(kind) => kind,
            None => {
                return Err(err(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "security.operation",
                ));
            }
        };
        self.require_known_peer(peer_key)?;
        if !self.security_available {
            return Err(err(
                BleErrorCode::PlatformSecurity,
                BleErrorDomain::Platform,
                "security.unavailable",
            ));
        }
        let id = self.admit_op(peer_key, timeout_ms, now, out)?;
        self.op_peers.push((id.clone(), String::from(peer_key)));
        match self.security_position(peer_key) {
            Some(index) => {
                self.security[index].state = PairState::Pairing;
                self.security[index].op = Some(id.clone());
            }
            None => {
                self.security.push(SecurityExchange {
                    peer_key: String::from(peer_key),
                    state: PairState::Pairing,
                    op: Some(id.clone()),
                });
            }
        }
        self.stage_effect(kind, &id, "security.request");
        Ok(id)
    }

    /// Settle a pairing exchange. Returns the outcome word.
    pub fn settle_security(&mut self, peer_key: &str, paired: bool) -> Result<String, CoreError> {
        let index = self.security_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "security.exchange",
            )
        })?;
        if self.security[index].state != PairState::Pairing {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "security.exchange",
            ));
        }
        let word = if paired { "paired" } else { "unpaired" };
        self.security[index].state = if paired {
            PairState::Paired
        } else {
            PairState::Unpaired
        };
        Ok(String::from(word))
    }

    /// Cancel a pairing exchange. A won bond stays won: cancelling after the
    /// peer paired reports `paired`, consistent with the pair result rather
    /// than contradicting it.
    pub fn cancel_pairing(&mut self, peer_key: &str) -> Result<String, CoreError> {
        let index = self.security_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "security.exchange",
            )
        })?;
        let word = match self.security[index].state {
            PairState::Paired => "paired",
            PairState::Pairing => {
                self.security[index].state = PairState::Cancelled;
                "cancelled"
            }
            PairState::Cancelled => "cancelled",
            PairState::Unpaired => "unpaired",
        };
        Ok(String::from(word))
    }

    /// Subscribe one consumer (GATT-03): the first consumer enables the one
    /// physical CCCD under the UBM owner; further consumers borrow the
    /// enablement with independent bounded streams.
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe(
        &mut self,
        path_index: usize,
        policy: &str,
        item_capacity: u64,
        byte_capacity: u64,
        consumer: &str,
        timeout_ms: u64,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<OperationId, CoreError> {
        self.check_effect_room()?;
        let Some(parsed_policy) = OverflowPolicy::from_str(policy) else {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "subscribe.policy",
            ));
        };
        if consumer.is_empty() {
            return Err(err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "subscribe.consumer",
            ));
        }
        self.check_path_fresh(path_index)?;
        self.require_gatt_link(path_index, "subscribe")?;
        let notify = self
            .paths
            .get(path_index)
            .map(|path| path.properties & (GATT_PROP_NOTIFY | GATT_PROP_INDICATE) != 0);
        if notify != Some(true) {
            return Err(err(
                BleErrorCode::GattPropertyNotSupported,
                BleErrorDomain::Gatt,
                "subscribe",
            ));
        }
        let limits = StreamLimits::new(
            item_capacity,
            byte_capacity,
            RESERVED_CONTROL_CAPACITY,
            RESERVED_CONTROL_BYTES,
        )?;
        self.sweep_reclaimable_hubs();
        let hub_index = match self.hub_position(path_index) {
            Some(index) => index,
            None => {
                if self.hubs.len() >= self.config.max_subscriptions {
                    return Err(err(
                        BleErrorCode::StreamQuota,
                        BleErrorDomain::Stream,
                        "subscribe.hubs",
                    ));
                }
                self.hubs.push(SubscriptionHub {
                    path_index,
                    physical: CccdPhysical::Disabled,
                    enable_op: None,
                    disable_op: None,
                    consumers: Vec::new(),
                });
                self.hubs.len() - 1
            }
        };
        if self.hubs[hub_index].physical == CccdPhysical::Invalid {
            return Err(err(
                BleErrorCode::GattStaleHandle,
                BleErrorDomain::Gatt,
                "subscribe.invalid",
            ));
        }
        if self.hubs[hub_index].physical == CccdPhysical::Disabling {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "subscribe.disabling",
            ));
        }
        // Bound the consumer list (orphan-disable ticket): drop same-lease
        // terminal records whose overflow terminal was observed (or never
        // existed) before the cap check, so subscribe → overflow → take →
        // unsubscribe cycles never grow the hub. An untaken overflow
        // terminal is preserved: the host still owns that observation.
        self.hubs[hub_index].consumers.retain(|record| {
            !(record.lease == consumer
                && record.state.is_terminal()
                && (record.terminal_taken || record.terminal.is_none()))
        });
        let known_op = self.hubs[hub_index]
            .consumers
            .iter()
            .find(|consumer_record| {
                consumer_record.lease == consumer && !consumer_record.state.is_terminal()
            })
            .and_then(|consumer_record| consumer_record.op.clone());
        if let Some(op) = known_op {
            return Ok(op);
        }
        if self.hubs[hub_index].consumers.len() >= self.config.max_consumers_per_subscription {
            return Err(err(
                BleErrorCode::StreamQuota,
                BleErrorDomain::Stream,
                "subscribe.consumers",
            ));
        }
        let id = self.admit_op(consumer, timeout_ms, now, out)?;
        let physical = self.hubs[hub_index].physical;
        if physical == CccdPhysical::Enabled {
            let generation = self.kernel_generation.clone();
            let ordinal = self.next_ordinal();
            self.dispatch_before_success(&id, ContenderKind::Success, out)?;
            self.kernel.handle(
                KernelInput::Complete {
                    operation_id: id.clone(),
                    generation,
                    contender: Contender {
                        ingress_ordinal: ordinal,
                        kind: ContenderKind::Success,
                        valid: true,
                    },
                },
                now,
                out,
            )?;
            self.hubs[hub_index].consumers.push(ConsumerRecord {
                lease: String::from(consumer),
                state: ConsumerState::Ready,
                op: Some(id.clone()),
                stream: Stream::new(limits, parsed_policy),
                terminal: None,
                terminal_taken: false,
                quarantined: 0,
                delivered: 0,
                slots: VecDeque::new(),
            });
            return Ok(id);
        }
        self.hubs[hub_index].consumers.push(ConsumerRecord {
            lease: String::from(consumer),
            state: ConsumerState::Enabling,
            op: Some(id.clone()),
            stream: Stream::new(limits, parsed_policy),
            terminal: None,
            terminal_taken: false,
            quarantined: 0,
            delivered: 0,
            slots: VecDeque::new(),
        });
        if physical == CccdPhysical::Disabled || physical == CccdPhysical::Failed {
            self.hubs[hub_index].physical = CccdPhysical::Enabling;
            self.hubs[hub_index].enable_op = Some(id.clone());
            self.stage_effect(CentralEffectKind::SubscribeEnable, &id, "subscribe.enable");
        }
        Ok(id)
    }

    /// Settle the physical CCCD enablement. Only `Enabling` consumers
    /// whose op is still live ride the outcome: a cancelled or otherwise
    /// settled op never revives its consumer (F12). A late success with no
    /// eligible consumer left stages a real compensating physical disable
    /// and holds `Disabling` until the OS confirms: no orphan live stream
    /// (CLN-03), and never a premature `Disabled` report. The enable ticket
    /// is consumed by the settle: a duplicate settle fails closed instead
    /// of corrupting the hub.
    pub fn settle_subscribe_enable(
        &mut self,
        path_index: usize,
        success: bool,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        let hub_index = self.hub_position(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "subscribe.hub",
            )
        })?;
        let enable_op = self.hubs[hub_index].enable_op.clone();
        if enable_op.is_none() {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "subscribe.enable",
            ));
        }
        // Snapshot eligibility before settling ops: an `Enabling` consumer
        // owns a live op exactly when it still waits on this enablement.
        let eligible: Vec<OperationId> = self.hubs[hub_index]
            .consumers
            .iter()
            .filter(|consumer| consumer.state == ConsumerState::Enabling)
            .filter_map(|consumer| consumer.op.clone())
            .filter(|op| {
                matches!(
                    self.kernel.operation_state(op),
                    Some(OpStateView::Queued) | Some(OpStateView::Dispatched)
                )
            })
            .collect();
        // A success with nobody eligible needs a compensating disable: the
        // OS enable is live with no owner. Reserve effect room and kernel
        // admission BEFORE any mutation so a full ledger fails closed and
        // the host can retry the settle.
        let compensating = if success && eligible.is_empty() {
            self.check_effect_room()?;
            let owner = match self.hubs[hub_index].consumers.first() {
                Some(consumer) => consumer.lease.clone(),
                None => String::from("subscribe-cleanup"),
            };
            Some(self.admit_op(&owner, 5000, now, out)?)
        } else {
            None
        };
        let ordinal = self.next_ordinal();
        let generation = self.kernel_generation.clone();
        let live_ops: Vec<OperationId> = self.hubs[hub_index]
            .consumers
            .iter()
            .filter_map(|consumer| consumer.op.clone())
            .collect();
        for op in live_ops.iter() {
            if self.kernel.operation_state(op).is_none() {
                continue;
            }
            let live = matches!(
                self.kernel.operation_state(op),
                Some(OpStateView::Queued) | Some(OpStateView::Dispatched)
            );
            if !live {
                continue;
            }
            let kind = if success {
                ContenderKind::Success
            } else {
                ContenderKind::Failure
            };
            // Best-effort like the settlement below: a queued success
            // dispatches first per the operation machine, a queued failure
            // settles directly.
            let _ = self.dispatch_before_success(op, kind, out);
            let contender = Contender {
                ingress_ordinal: ordinal,
                kind,
                valid: true,
            };
            let _ = self.kernel.handle(
                KernelInput::Complete {
                    operation_id: op.clone(),
                    generation: generation.clone(),
                    contender,
                },
                now,
                out,
            );
        }
        if success {
            if let Some(disable_id) = compensating {
                self.hubs[hub_index].physical = CccdPhysical::Disabling;
                self.hubs[hub_index].enable_op = None;
                self.hubs[hub_index].disable_op = Some(disable_id.clone());
                self.stage_effect(
                    CentralEffectKind::SubscribeDisable,
                    &disable_id,
                    "subscribe.disable",
                );
                for consumer in self.hubs[hub_index].consumers.iter_mut() {
                    if consumer.state == ConsumerState::Removing
                        || consumer.state == ConsumerState::Enabling
                    {
                        consumer.state = ConsumerState::Removed;
                    }
                }
                return Ok(());
            }
            self.hubs[hub_index].physical = CccdPhysical::Enabled;
            self.hubs[hub_index].enable_op = None;
            for consumer in self.hubs[hub_index].consumers.iter_mut() {
                if consumer.state != ConsumerState::Enabling {
                    continue;
                }
                let rides = match &consumer.op {
                    Some(op) => eligible.contains(op),
                    None => false,
                };
                consumer.state = if rides {
                    ConsumerState::Ready
                } else {
                    ConsumerState::Removed
                };
            }
            return Ok(());
        }
        self.hubs[hub_index].physical = CccdPhysical::Failed;
        self.hubs[hub_index].enable_op = None;
        for consumer in self.hubs[hub_index].consumers.iter_mut() {
            if consumer.state == ConsumerState::Enabling {
                consumer.state = ConsumerState::Failed;
            }
        }
        Ok(())
    }

    /// Remove one consumer. Removing one consumer never drops another's CCCD:
    /// returns true only when the last removal issues the physical disable.
    /// A terminal consumer (`Failed` after overflow, `Invalid` after a
    /// service change, `Removed`) prunes its record to bound the consumer
    /// list and still releases an orphan live CCCD: when no live consumer
    /// remains but the hub is physically enabled, the removal issues the
    /// physical disable instead of leaking it (orphan-disable ticket).
    pub fn unsubscribe(
        &mut self,
        path_index: usize,
        consumer: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<bool, CoreError> {
        self.check_effect_room()?;
        let hub_index = match self.hub_position(path_index) {
            Some(index) => index,
            None => return Ok(false),
        };
        let consumer_index = match self.hubs[hub_index]
            .consumers
            .iter()
            .position(|known| known.lease == consumer)
        {
            Some(index) => index,
            None => return Ok(false),
        };
        match self.hubs[hub_index].consumers[consumer_index].state {
            ConsumerState::Enabling | ConsumerState::Ready => {}
            ConsumerState::Removing => return Ok(false),
            ConsumerState::Failed | ConsumerState::Invalid | ConsumerState::Removed => {
                return self.remove_terminal_consumer(hub_index, consumer_index, now, out);
            }
        }
        self.hubs[hub_index].consumers[consumer_index].state = ConsumerState::Removing;
        self.issue_physical_disable(hub_index, consumer, now, out)
    }

    /// Shared tail of [`Central::unsubscribe`]: issue the physical disable
    /// only for the last live removal on an enabled hub.
    fn issue_physical_disable(
        &mut self,
        hub_index: usize,
        consumer: &str,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<bool, CoreError> {
        let active = self.hubs[hub_index]
            .consumers
            .iter()
            .any(|known| matches!(known.state, ConsumerState::Enabling | ConsumerState::Ready));
        if self.hubs[hub_index].physical != CccdPhysical::Enabled || active {
            return Ok(false);
        }
        let id = self.admit_op(consumer, 5000, now, out)?;
        self.hubs[hub_index].physical = CccdPhysical::Disabling;
        self.hubs[hub_index].disable_op = Some(id.clone());
        self.stage_effect(
            CentralEffectKind::SubscribeDisable,
            &id,
            "subscribe.disable",
        );
        Ok(true)
    }

    /// Remove a terminal consumer record and release an orphan live CCCD.
    /// An untaken overflow terminal keeps its record: the host still owns
    /// that observation, and takes it after the disable settles. Either
    /// way the physical disable fires when no live consumer remains.
    fn remove_terminal_consumer(
        &mut self,
        hub_index: usize,
        consumer_index: usize,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<bool, CoreError> {
        let lease = self.hubs[hub_index].consumers[consumer_index].lease.clone();
        let keep_for_terminal = {
            let record = &self.hubs[hub_index].consumers[consumer_index];
            record.state == ConsumerState::Failed
                && record.terminal.is_some()
                && !record.terminal_taken
        };
        if !keep_for_terminal {
            self.hubs[hub_index].consumers.remove(consumer_index);
        }
        self.issue_physical_disable(hub_index, &lease, now, out)
    }

    /// Settle the physical CCCD disablement.
    pub fn settle_subscribe_disable(
        &mut self,
        path_index: usize,
        now: MonotonicTime,
        out: &mut EffectBatch,
    ) -> Result<(), CoreError> {
        let hub_index = self.hub_position(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "subscribe.hub",
            )
        })?;
        if self.hubs[hub_index].physical == CccdPhysical::Disabled {
            return Ok(());
        }
        if self.hubs[hub_index].physical != CccdPhysical::Disabling {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "subscribe.disable",
            ));
        }
        let ordinal = self.next_ordinal();
        let generation = self.kernel_generation.clone();
        if let Some(op) = self.hubs[hub_index].disable_op.clone() {
            let live = matches!(
                self.kernel.operation_state(&op),
                Some(OpStateView::Queued) | Some(OpStateView::Dispatched)
            );
            if live {
                self.dispatch_before_success(&op, ContenderKind::Success, out)?;
                self.kernel.handle(
                    KernelInput::Complete {
                        operation_id: op,
                        generation,
                        contender: Contender {
                            ingress_ordinal: ordinal,
                            kind: ContenderKind::Success,
                            valid: true,
                        },
                    },
                    now,
                    out,
                )?;
            }
        }
        self.hubs[hub_index].physical = CccdPhysical::Disabled;
        for consumer in self.hubs[hub_index].consumers.iter_mut() {
            if consumer.state == ConsumerState::Removing {
                consumer.state = ConsumerState::Removed;
            }
        }
        Ok(())
    }

    /// Deliver one native notification to every consumer of a path (GATT-04):
    /// pre-ready values quarantine, ready values enter bounded streams,
    /// removed/terminated consumers receive nothing. Returns per-consumer
    /// outcomes in subscription order. Length-only accounting: no value is
    /// buffered (see [`Central::deliver_notification_value`] for the
    /// value-carrying path).
    pub fn deliver_notification(
        &mut self,
        path_index: usize,
        value_len: u64,
    ) -> Result<Vec<(String, DeliveryOutcome)>, CoreError> {
        self.deliver_inner(path_index, value_len, None)
    }

    /// Deliver one native notification value to every consumer of a path:
    /// the same accounting as [`Central::deliver_notification`], plus the
    /// bytes buffer per stream-admitted consumer for later
    /// [`Central::take_notification_value`]. Values buffer on `Admit` and
    /// on retaining (`Replace`/`DropOldest`) decisions, after the stream
    /// enforces its item/byte bounds, so an oversized or over-capacity
    /// value drops or terminates instead of allocating unbounded.
    pub fn deliver_notification_value(
        &mut self,
        path_index: usize,
        value: &[u8],
    ) -> Result<Vec<(String, DeliveryOutcome)>, CoreError> {
        let value_len = value.len() as u64;
        self.deliver_inner(path_index, value_len, Some(value))
    }

    fn deliver_inner(
        &mut self,
        path_index: usize,
        value_len: u64,
        value: Option<&[u8]>,
    ) -> Result<Vec<(String, DeliveryOutcome)>, CoreError> {
        let hub_index = self.hub_position(path_index).ok_or_else(|| {
            err(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "subscribe.hub",
            )
        })?;
        let mut outcomes: Vec<(String, DeliveryOutcome)> =
            Vec::with_capacity(self.hubs[hub_index].consumers.len());
        for consumer in self.hubs[hub_index].consumers.iter_mut() {
            let outcome = match consumer.state {
                ConsumerState::Removed => DeliveryOutcome::DroppedRemoved,
                ConsumerState::Invalid | ConsumerState::Failed => DeliveryOutcome::DroppedLate,
                ConsumerState::Enabling => {
                    consumer.quarantined = consumer.quarantined.saturating_add(1);
                    DeliveryOutcome::QuarantinedPreReady
                }
                ConsumerState::Ready | ConsumerState::Removing => {
                    if consumer.terminal_taken {
                        DeliveryOutcome::DroppedLate
                    } else {
                        match consumer.stream.push_data(value_len) {
                            Ok(push) => match push.decision {
                                crate::streams::AdmissionDecision::Admit => {
                                    consumer.delivered = consumer.delivered.saturating_add(1);
                                    consumer.slots.push_back(value.map(<[u8]>::to_vec));
                                    DeliveryOutcome::Delivered
                                }
                                crate::streams::AdmissionDecision::Replace
                                | crate::streams::AdmissionDecision::DropOldest => {
                                    // Retention and accounting are one atomic
                                    // queue operation: drop exactly the slots
                                    // the ledger evicted, then retain the
                                    // newest alongside its ledger entry.
                                    consumer.delivered = consumer.delivered.saturating_add(1);
                                    let dropped =
                                        (push.evicted_items as usize).min(consumer.slots.len());
                                    consumer.slots.drain(0..dropped);
                                    consumer.slots.push_back(value.map(<[u8]>::to_vec));
                                    DeliveryOutcome::OverflowNoticed
                                }
                                crate::streams::AdmissionDecision::DropNewest => {
                                    DeliveryOutcome::OverflowNoticed
                                }
                                crate::streams::AdmissionDecision::Terminate => {
                                    consumer.terminal = Some(SubscriptionTerminal {
                                        dropped_items: 1,
                                        dropped_bytes: value_len,
                                        replaced_items: 0,
                                    });
                                    consumer.state = ConsumerState::Failed;
                                    DeliveryOutcome::Delivered
                                }
                            },
                            Err(_) => DeliveryOutcome::DroppedLate,
                        }
                    }
                }
            };
            outcomes.push((consumer.lease.clone(), outcome));
        }
        Ok(outcomes)
    }

    /// Take one buffered notification value for a consumer (FIFO arrival
    /// order). `Ready`, `Removing`, and overflow-`Failed` consumers
    /// release values: bytes admitted before the terminal stay valid
    /// observations (drain, then take the terminal). `Invalid`/`Removed`
    /// consumers observe `None` so stale values never cross invalidation.
    /// Popping frees the stream bytes so the bound recycles. Length-only
    /// slots carry no bytes: takes skip past them (freeing their ledger
    /// sizes) so every returned value pairs with its own size.
    pub fn take_notification_value(
        &mut self,
        path_index: usize,
        consumer: &str,
    ) -> Option<Vec<u8>> {
        let hub_index = self.hub_position(path_index)?;
        let record = self.hubs[hub_index]
            .consumers
            .iter_mut()
            .find(|known| known.lease == consumer)?;
        match record.state {
            ConsumerState::Ready | ConsumerState::Removing | ConsumerState::Failed => {}
            _ => return None,
        }
        loop {
            match record.slots.pop_front() {
                None => return None,
                Some(None) => {
                    record.stream.release_oldest_data(1);
                }
                Some(Some(value)) => {
                    record.stream.release_oldest_data(1);
                    return Some(value);
                }
            }
        }
    }

    /// Buffered values waiting for a consumer (observation without drain).
    /// Length-only slots hold no bytes and are not counted.
    #[must_use]
    pub fn pending_value_count(&self, path_index: usize, consumer: &str) -> Option<u64> {
        let hub_index = self.hub_position(path_index)?;
        self.hubs[hub_index]
            .consumers
            .iter()
            .find(|known| known.lease == consumer)
            .map(|record| record.slots.iter().filter(|slot| slot.is_some()).count() as u64)
    }

    /// Take one consumer's terminal overflow event, if present. A terminal
    /// occurs once; afterwards the consumer is closed to late values.
    pub fn take_terminal(
        &mut self,
        path_index: usize,
        consumer: &str,
    ) -> Option<SubscriptionTerminal> {
        let hub_index = self.hub_position(path_index)?;
        let consumer_record = self.hubs[hub_index]
            .consumers
            .iter_mut()
            .find(|known| known.lease == consumer)?;
        if consumer_record.terminal_taken {
            return None;
        }
        let terminal = consumer_record.terminal?;
        consumer_record.terminal_taken = true;
        Some(terminal)
    }

    /// Observe one consumer's state, if present.
    #[must_use]
    pub fn consumer_state(&self, path_index: usize, consumer: &str) -> Option<ConsumerState> {
        self.hub_position(path_index).and_then(|hub_index| {
            self.hubs[hub_index]
                .consumers
                .iter()
                .find(|known| known.lease == consumer)
                .map(|known| known.state)
        })
    }

    /// Whether the physical CCCD is currently enabled for a path.
    #[must_use]
    pub fn physical_cccd_enabled(&self, path_index: usize) -> bool {
        self.hub_position(path_index)
            .map(|hub_index| self.hubs[hub_index].physical == CccdPhysical::Enabled)
            .unwrap_or(false)
    }

    /// Quarantined pre-ready values for one consumer (GATT-04 evidence).
    #[must_use]
    pub fn quarantined_count(&self, path_index: usize, consumer: &str) -> Option<u64> {
        self.hub_position(path_index).and_then(|hub_index| {
            self.hubs[hub_index]
                .consumers
                .iter()
                .find(|known| known.lease == consumer)
                .map(|known| known.quarantined)
        })
    }

    /// One incremental destroy step (F15): settle some queued work as
    /// destroyed and request release for dispatched work, staging effects
    /// into `out` (bounded by the batch and `max_effects_per_call`). The host
    /// executes `out` outside the core lock — settling dispatched work via
    /// `settle_op(Destroy)` when needed and acknowledging every terminal via
    /// `report_release_success/failure` — drains `out`, and calls again until
    /// `progress.done`. Effects are never dropped on the floor: a truncated
    /// step reports progress and the host resumes with a drained batch.
    pub fn destroy_step(&mut self, out: &mut EffectBatch) -> Result<DestroyProgress, CoreError> {
        if self.destroy_record.is_some() {
            // Already finalized: no further effects, done when nothing pends.
            let live = self.live_operation_ids().len();
            let terminal = self.terminal_operation_ids().len();
            return Ok(DestroyProgress {
                settled_queued: 0,
                truncated: false,
                live_operations: live,
                terminal_pending_release: terminal,
                done: live == 0 && terminal == 0,
            });
        }
        let (settled_queued, truncated) = match self.kernel.handle(KernelInput::Shutdown, 0, out)? {
            HandleOutcome::ShutDown {
                settled_queued,
                truncated,
                ..
            } => (settled_queued, truncated),
            _ => {
                return Err(err(
                    BleErrorCode::LifecycleInvalidState,
                    BleErrorDomain::Core,
                    "central.destroy.unexpected",
                ));
            }
        };
        let live = self.live_operation_ids().len();
        let terminal = self.terminal_operation_ids().len();
        Ok(DestroyProgress {
            settled_queued,
            truncated,
            live_operations: live,
            terminal_pending_release: terminal,
            done: !truncated && live == 0 && terminal == 0,
        })
    }

    /// Final authoritative cleanup record, available only after incremental
    /// destruction completes (no live work and no terminal pending release).
    /// Returns `lifecycle.invalid-state` while work pends; caches and returns
    /// the same record on repeat calls. `Released` only when disconnect
    /// failures and retained release failures are all absent; otherwise
    /// `ReleaseFailed` with every failure preserved.
    pub fn destroy_record(&mut self) -> Result<CleanupRecord, CoreError> {
        if let Some(record) = self.destroy_record.clone() {
            return Ok(record);
        }
        if !self.live_operation_ids().is_empty() || !self.terminal_operation_ids().is_empty() {
            return Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.destroy.pending",
            ));
        }
        let mut failures: Vec<CleanupFailure> = Vec::new();
        for retained in self.disconnect_failures.iter() {
            for failure in retained.failures().iter() {
                failures.push(failure.clone());
            }
        }
        for retained in self.kernel.drain_cleanup(usize::MAX) {
            if retained.state() == CleanupState::ReleaseFailed {
                for failure in retained.failures().iter() {
                    failures.push(failure.clone());
                }
            }
        }
        let state = if failures.is_empty() {
            CleanupState::Released
        } else {
            CleanupState::ReleaseFailed
        };
        let record = CleanupRecord::new(None, state, failures)?;
        self.destroy_record = Some(record.clone());
        Ok(record)
    }

    /// Destroy (OPS-04/CLN-05): admission stops, queued work settles as
    /// destroyed with bounded termination, and one authoritative record is
    /// returned. A second destroy returns the same record; retained
    /// disconnect failures are reported, never bypassed.
    ///
    /// Legacy convenience for hosts that cannot drive incremental steps
    /// (e.g. process teardown with no live radio work): loops with a drained
    /// batch so a filled batch recycles instead of truncating forever (F15).
    /// Hosts with live dispatched work should prefer `destroy_step` plus
    /// explicit per-op acknowledgements and `destroy_record`.
    pub fn destroy(&mut self, out: &mut EffectBatch) -> Result<CleanupRecord, CoreError> {
        if let Some(record) = self.destroy_record.clone() {
            return Ok(record);
        }
        let mut budget = self.op_ids.len().saturating_add(1);
        loop {
            match self.kernel.handle(KernelInput::Shutdown, 0, out)? {
                HandleOutcome::ShutDown { truncated, .. } => {
                    if !truncated {
                        break;
                    }
                }
                _ => {
                    return Err(err(
                        BleErrorCode::LifecycleInvalidState,
                        BleErrorDomain::Core,
                        "central.destroy.unexpected",
                    ));
                }
            }
            // F15: recycle the batch so the next pass has room. The caller
            // observes only the final pass's effects; incremental hosts use
            // `destroy_step` to execute every batch.
            let _ = out.drain();
            if budget == 0 {
                return Err(err(
                    BleErrorCode::StreamQuota,
                    BleErrorDomain::Stream,
                    "central.destroy.truncated",
                ));
            }
            budget -= 1;
        }
        let mut failures: Vec<CleanupFailure> = Vec::new();
        for retained in self.disconnect_failures.iter() {
            for failure in retained.failures().iter() {
                failures.push(failure.clone());
            }
        }
        let state = if failures.is_empty() {
            CleanupState::Released
        } else {
            CleanupState::ReleaseFailed
        };
        let record = CleanupRecord::new(None, state, failures)?;
        self.destroy_record = Some(record.clone());
        Ok(record)
    }

    /// Drop central-side tracking for one released operation.
    fn prune_released_op(&mut self, id: &OperationId) {
        self.op_ids.retain(|known| known != id);
        self.op_paths.retain(|(known, _)| known != id);
        self.op_peers.retain(|(known, _)| known != id);
    }

    /// Report a host release result for a terminal operation. A failed
    /// release is retained and reported, never a silent clean release.
    pub fn report_release_failure(
        &mut self,
        id: &OperationId,
        code: BleErrorCode,
    ) -> Result<(), CoreError> {
        match self.kernel.handle(
            KernelInput::ReleaseReport {
                operation_id: id.clone(),
                ok: false,
                code: Some(code),
            },
            0,
            &mut EffectBatch::new(1),
        )? {
            HandleOutcome::ReleaseRecorded { .. } => {
                self.prune_released_op(id);
                Ok(())
            }
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.release.unexpected",
            )),
        }
    }

    /// Report a successful host release for a terminal operation. The
    /// kernel reaps the entry and central prunes its operation tracking,
    /// so aggregate admission reclaims (M1 liveness).
    pub fn report_release_success(&mut self, id: &OperationId) -> Result<(), CoreError> {
        match self.kernel.handle(
            KernelInput::ReleaseReport {
                operation_id: id.clone(),
                ok: true,
                code: None,
            },
            0,
            &mut EffectBatch::new(1),
        )? {
            HandleOutcome::ReleaseRecorded { .. } => {
                self.prune_released_op(id);
                Ok(())
            }
            _ => Err(err(
                BleErrorCode::LifecycleInvalidState,
                BleErrorDomain::Core,
                "central.release.unexpected",
            )),
        }
    }

    /// Drain retained kernel cleanup records, up to `max`. Retained
    /// disconnect failures stay owned by a later destroy: draining never
    /// bypasses them (CLN-01).
    pub fn drain_cleanup(&mut self, max: usize) -> Vec<CleanupRecord> {
        self.kernel.drain_cleanup(max)
    }

    /// A borrowing manager exits (OWN-05): its leases release while the
    /// process-owned restoration authority survives.
    pub fn release_borrower(&mut self, peer_key: &str, lease: &str) -> Result<(), CoreError> {
        let index = self.connection_position(peer_key).ok_or_else(|| {
            err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.borrower",
            )
        })?;
        let held = self.connections[index]
            .leases
            .iter()
            .any(|known| known == lease);
        if !held {
            return Err(err(
                BleErrorCode::ConnectionNotFound,
                BleErrorDomain::Connection,
                "connection.borrower",
            ));
        }
        self.connections[index]
            .leases
            .retain(|known| known != lease);
        if self.connections[index].leases.is_empty() && !self.connections[index].state.is_terminal()
        {
            let state = self.connections[index].state;
            let next = step_connection(state, ConnectionEvent::Disconnect)?;
            self.connections[index].state = next;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check;
    use crate::contracts::{
        AdapterGeneration, AdapterId, AttachmentId, BackendGeneration, BackendInstanceId,
    };

    fn fixture_attachment() -> Result<AttachmentTuple, CoreError> {
        Ok(AttachmentTuple::new(
            AttachmentId::new("attach-01")?,
            BackendInstanceId::new("backend-01")?,
            BackendGeneration::new("bg-3")?,
            AdapterId::new("adapter-01")?,
            AdapterGeneration::new("ag-2")?,
        ))
    }

    fn other_attachment() -> Result<AttachmentTuple, CoreError> {
        Ok(AttachmentTuple::new(
            AttachmentId::new("attach-09")?,
            BackendInstanceId::new("backend-09")?,
            BackendGeneration::new("bg-9")?,
            AdapterId::new("adapter-09")?,
            AdapterGeneration::new("ag-9")?,
        ))
    }

    fn fixture_central() -> Result<Central, CoreError> {
        Central::new(
            fixture_attachment()?,
            Generation::new("kernel-gen-1")?,
            CentralConfig::default(),
        )
    }

    fn batch() -> EffectBatch {
        EffectBatch::new(64)
    }

    fn expect_code<T>(
        result: Result<T, CoreError>,
        code: BleErrorCode,
        domain: BleErrorDomain,
    ) -> Result<(), CoreError> {
        match result {
            Err(error) => {
                check(error.code() == code, "error code identity");
                check(error.domain() == domain, "error domain identity");
                Ok(())
            }
            Ok(_) => {
                check(false, "expected rejection, got success");
                Ok(())
            }
        }
    }

    /// Connect, discover, and register one notifiable characteristic path.
    fn live_characteristic(
        central: &mut Central,
        out: &mut EffectBatch,
    ) -> Result<(String, usize), CoreError> {
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let op = central.connect(&peer, "client-1", 5000, 1000, out)?;
        central.dispatch_op(&op, out)?;
        central.settle_op(&op, ContenderKind::Success, true, 0, 1001, out)?;
        central.note_link_established(&peer)?;
        central.begin_discovery(&peer)?;
        central.complete_discovery(&peer)?;
        let path = central.register_path(
            &peer,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_WRITE | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        Ok((peer, path))
    }

    #[test]
    fn uuid_canonicalizes_16_bit_forms() -> Result<(), CoreError> {
        let canonical = canonical_uuid("180D")?;
        check(
            canonical == "0000180d-0000-1000-8000-00805f9b34fb",
            "16-bit expansion",
        );
        let dashed = canonical_uuid("0000180D-0000-1000-8000-00805F9B34FB")?;
        check(dashed == canonical, "128-bit round trip");
        expect_code(
            canonical_uuid("bogus"),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            canonical_uuid(""),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn scan_request_accepts_explicit_shape() -> Result<(), CoreError> {
        let request = validate_scan_request(&["180D"], "all", "none", 5000, true, &[])?;
        check(request.timeout_ms() == 5000, "timeout kept");
        check(request.has_abort_signal(), "abort shape kept");
        check(
            request.duplicate() == ScanDuplicatePolicy::All,
            "duplicate kept",
        );
        check(
            request.service_uuids() == [String::from("0000180d-0000-1000-8000-00805f9b34fb")],
            "filter canonicalized",
        );
        let broad = validate_scan_request(&[], "first", "latest-by-timestamp", 100, false, &[])?;
        check(
            broad.service_uuids().is_empty(),
            "empty filter is broad scan",
        );
        Ok(())
    }

    #[test]
    fn scan_residuals_and_bad_fields_fail_closed() -> Result<(), CoreError> {
        expect_code(
            validate_scan_request(&["180D"], "all", "none", 5000, false, &["raw-bytes"]),
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            validate_scan_request(&["bogus"], "all", "none", 5000, false, &[]),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            validate_scan_request(&[], "all", "none", 0, false, &[]),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            validate_scan_request(&[], "every", "none", 5000, false, &[]),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            validate_scan_request(&[], "all", "sometimes", 5000, false, &[]),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn scan_tables_reject_off_table_steps() -> Result<(), CoreError> {
        let active = step_scan_session(
            ScanSessionState::Starting,
            ScanPlatformEvent::PlatformStarted,
        )?;
        check(active == ScanSessionState::Active, "platform start");
        expect_code(
            step_scan_session(ScanSessionState::Stopped, ScanPlatformEvent::Stop),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )?;
        expect_code(
            step_scan_session(ScanSessionState::Failed, ScanPlatformEvent::PlatformStarted),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn second_physical_scan_rejected_first_unchanged() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let request = validate_scan_request(&["180D"], "all", "none", 5000, false, &[])?;
        let first = central.start_scan(&request, None, "owner-a", 1000, &mut out)?;
        central.platform_scan_started(&first)?;
        let before = out.len();
        expect_code(
            central.start_scan(&request, None, "owner-b", 1001, &mut out),
            BleErrorCode::ScanAlreadyActive,
            BleErrorDomain::Core,
        )?;
        check(out.len() == before, "rejection stages no effects");
        check(
            central.scan_session_state(&first) == Some(ScanSessionState::Active),
            "first scan unchanged",
        );
        Ok(())
    }

    #[test]
    fn shared_scan_gets_independent_lease_stream() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let request =
            validate_scan_request(&[], "merged", "latest-by-timestamp", 5000, false, &[])?;
        let first = central.start_scan(&request, None, "owner-a", 1000, &mut out)?;
        central.platform_scan_started(&first)?;
        let shared = central.start_scan(&request, Some("token-7"), "owner-b", 1001, &mut out)?;
        check(shared != first, "independent operation");
        check(
            central.scan_session_state(&shared) == Some(ScanSessionState::Starting),
            "shared session admitted",
        );
        Ok(())
    }

    #[test]
    fn scan_lifecycle_stop_needs_platform_confirm() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let request = validate_scan_request(&[], "all", "none", 5000, false, &[])?;
        let id = central.start_scan(&request, None, "owner-a", 1000, &mut out)?;
        central.platform_scan_started(&id)?;
        central.stop_scan(&id, 1002, &mut out)?;
        check(
            central.scan_session_state(&id) == Some(ScanSessionState::Stopping),
            "stop requested",
        );
        let state =
            central.note_scan_platform(&id, ScanPlatformEvent::PlatformStopped, 1003, &mut out)?;
        check(state == ScanSessionState::Stopped, "platform confirmed");
        check(state.is_terminal(), "stopped is terminal");
        Ok(())
    }

    #[test]
    fn scan_start_failure_reaches_failed() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let request = validate_scan_request(&[], "all", "none", 5000, false, &[])?;
        let id = central.start_scan(&request, None, "owner-a", 1000, &mut out)?;
        let state =
            central.note_scan_platform(&id, ScanPlatformEvent::StartFailed, 1001, &mut out)?;
        check(state == ScanSessionState::Failed, "start failed");
        Ok(())
    }

    #[test]
    fn peer_resolution_keys_and_rejects() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let key = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:FF")?;
        check(key == "public-address:AA:BB:CC:DD:EE:FF", "session key");
        central.require_known_peer(&key)?;
        expect_code(
            central.resolve_peer("carrier-pigeon", "x"),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.resolve_peer("public-address", ""),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.require_known_peer("public-address:unknown"),
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
        )
    }

    #[test]
    fn own04_canonical_change_keeps_native_ref() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let old = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let before = central.known_peer_count();
        let new = central.update_peer_canonical(&old, "public-address", "AA:BB:CC:DD:EE:02")?;
        check(new == "public-address:AA:BB:CC:DD:EE:02", "new key");
        check(central.known_peer_count() == before, "no duplicate device");
        central.require_known_peer(&new)?;
        expect_code(
            central.require_known_peer(&old),
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
        )
    }

    #[test]
    fn own01_exclusive_second_connect_rejected() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        expect_code(
            central.connect(&peer, "client-2", 5000, 1001, &mut out),
            BleErrorCode::ConnectionAlreadyOwned,
            BleErrorDomain::Core,
        )?;
        check(central.connection_lease_count(&peer) == 1, "single owner");
        Ok(())
    }

    #[test]
    fn own01_shared_link_grants_borrow_lease() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_sharing_supported(true);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let _second = central.borrow_connection(&peer, "client-2", 5000, 1001, &mut out)?;
        check(central.connection_lease_count(&peer) == 2, "two leases");
        Ok(())
    }

    #[test]
    fn lease_transfer_validates_and_checks_generation() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_sharing_supported(true);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let generation = central.connection_generation(&peer).ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.generation",
            )
        })?;
        central.transfer_lease(&peer, "client-1", "client-9", &generation, 1)?;
        check(
            central.connection_lease_count(&peer) == 1,
            "lease moved, not copied",
        );
        expect_code(
            central.transfer_lease(&peer, "", "client-9", &generation, 1),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.transfer_lease(&peer, "client-9", "client-1", "cg-stale", 2),
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
        )
    }

    #[test]
    fn final_release_ends_link_explicit_disconnect_ends_with_borrowers() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_sharing_supported(true);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let _second = central.borrow_connection(&peer, "client-2", 5000, 1001, &mut out)?;
        central.disconnect(&peer, "client-1", 1002, &mut out)?;
        check(
            central.connection_state(&peer) == Some(ConnectionState::Disconnecting),
            "explicit disconnect starts release",
        );
        central.note_link_released(&peer)?;
        check(
            central.connection_state(&peer) == Some(ConnectionState::Disconnected),
            "owner disconnect ends link",
        );
        Ok(())
    }

    #[test]
    fn release_final_lease_ends_link() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let ended = central.release_lease(&peer, "client-1", 1001, &mut out)?;
        check(ended, "final release ends the link");
        check(central.connection_lease_count(&peer) == 0, "no leases left");
        central.note_link_released(&peer)?;
        check(
            central.connection_state(&peer) == Some(ConnectionState::Disconnected),
            "link released",
        );
        expect_code(
            central.release_lease(&peer, "client-1", 1002, &mut out),
            BleErrorCode::ConnectionNotFound,
            BleErrorDomain::Connection,
        )
    }

    #[test]
    fn own03_late_completion_cannot_touch_new_link() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let op_a = central.connect(&peer, "client-a", 5000, 1000, &mut out)?;
        let cancelled = central.cancel_op(&op_a, 1001, &mut out)?;
        match cancelled {
            CompletionOutcome::Settled { kind, .. } => {
                check(kind == OperationTerminalKind::Aborted, "A aborted");
            }
            _ => {
                check(false, "cancel must settle");
            }
        }
        let op_b = central.connect(&peer, "client-b", 5000, 1002, &mut out)?;
        central.dispatch_op(&op_b, &mut out)?;
        central.settle_op(&op_b, ContenderKind::Success, true, 3, 1003, &mut out)?;
        let late = central.settle_op(&op_a, ContenderKind::Success, true, 4, 1004, &mut out)?;
        match late {
            CompletionOutcome::DuplicateSuppressed { suppressed } => {
                check(suppressed == 1, "late A suppressed");
            }
            _ => {
                check(false, "late completion must suppress");
            }
        }
        check(
            central.operation_state(&op_b)
                == Some(OpStateView::Terminal(OperationTerminalKind::Succeeded)),
            "B untouched",
        );
        Ok(())
    }

    #[test]
    fn own02_foreign_handle_rejected_before_radio() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let stored = central.stored_path(path).ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.path",
            )
        })?;
        check(stored.service_occurrence() == 0, "path stored");
        expect_code(
            central.check_path_fresh(usize::MAX),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        // The scope rotates (adapter reset): the old handle is foreign now and
        // any use rejects before dispatch, staging nothing.
        central.handle_adapter_reset(
            other_attachment()?,
            Generation::new("kernel-gen-2")?,
            3000,
            &mut out,
        )?;
        let before_ops = central.live_operation_count();
        let before_effects = out.len();
        expect_code(
            central.check_path_fresh(path),
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
        )?;
        expect_code(
            central.start_read(path, 5000, 3001, &mut out),
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
        )?;
        check(
            central.live_operation_count() == before_ops,
            "no op admitted",
        );
        check(out.len() == before_effects, "no radio effect");
        Ok(())
    }

    #[test]
    fn own06_adapter_reset_invalidates_and_settles_truthfully() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let read = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let settled = central.handle_adapter_reset(
            other_attachment()?,
            Generation::new("kernel-gen-2")?,
            2001,
            &mut out,
        )?;
        check(settled >= 1, "live work settled");
        expect_code(
            central.check_path_fresh(path),
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
        )?;
        check(
            central.connection_state(&peer).is_none(),
            "old link cleared by reset",
        );
        // Fresh scope works again.
        let fresh = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:77")?;
        let _op = central.connect(&fresh, "client-1", 5000, 2002, &mut out)?;
        check(
            central.connection_state(&fresh) == Some(ConnectionState::Connecting),
            "new scope connects",
        );
        Ok(())
    }

    #[test]
    fn gatt01_occurrence_resolves_duplicates_uuid_only_ambiguous() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, first) = live_characteristic(&mut central, &mut out)?;
        let second = central.register_path(
            &peer,
            "180D",
            1,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ,
            "lease-1",
        )?;
        check(second == first + 1, "second occurrence stored");
        let resolved = central.resolve_path(
            &peer,
            &PathSelector {
                service_uuid: String::from("0000180d-0000-1000-8000-00805f9b34fb"),
                service_occurrence: Some(1),
                characteristic_uuid: Some(String::from("00002a37-0000-1000-8000-00805f9b34fb")),
                characteristic_occurrence: Some(0),
                descriptor_uuid: None,
                descriptor_occurrence: None,
            },
        )?;
        check(resolved == second, "occurrence selects");
        expect_code(
            central.resolve_path(
                &peer,
                &PathSelector {
                    service_uuid: String::from("0000180d-0000-1000-8000-00805f9b34fb"),
                    service_occurrence: None,
                    characteristic_uuid: Some(String::from("00002a37-0000-1000-8000-00805f9b34fb")),
                    characteristic_occurrence: Some(0),
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
            ),
            BleErrorCode::GattAmbiguousPath,
            BleErrorDomain::Gatt,
        )?;
        expect_code(
            central.resolve_path(
                &peer,
                &PathSelector {
                    service_uuid: String::from("00001800-0000-1000-8000-00805f9b34fb"),
                    service_occurrence: Some(0),
                    characteristic_uuid: None,
                    characteristic_occurrence: None,
                    descriptor_uuid: None,
                    descriptor_occurrence: None,
                },
            ),
            BleErrorCode::GattNotFound,
            BleErrorDomain::Gatt,
        )?;
        expect_code(
            central.register_path(
                &peer,
                "180D",
                2,
                Some("2A37"),
                None,
                None,
                None,
                GATT_PROP_READ,
                "lease-1",
            ),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )
    }

    /// F04 helper: the exact selector for `live_characteristic`'s path.
    fn f04_selector() -> PathSelector {
        PathSelector {
            service_uuid: String::from("0000180d-0000-1000-8000-00805f9b34fb"),
            service_occurrence: Some(0),
            characteristic_uuid: Some(String::from("00002a37-0000-1000-8000-00805f9b34fb")),
            characteristic_occurrence: Some(0),
            descriptor_uuid: None,
            descriptor_occurrence: None,
        }
    }

    /// F04 helper: one subscribe/enable/release/unsubscribe/disable/release
    /// cycle, releasing terminal ops the way a host does.
    fn f04_cycle(
        central: &mut Central,
        out: &mut EffectBatch,
        path: usize,
        lease: &str,
        base: u64,
    ) -> Result<(), CoreError> {
        let sub = central.subscribe(path, "drop-oldest", 4, 128, lease, 5000, base, out)?;
        central.settle_subscribe_enable(path, true, base + 1, out)?;
        central.report_release_success(&sub)?;
        check(
            central.consumer_state(path, lease) == Some(ConsumerState::Ready),
            "cycle consumer ready",
        );
        let disabled = central.unsubscribe(path, lease, base + 2, out)?;
        check(disabled, "cycle last removal disables");
        let hub = central
            .hubs
            .iter()
            .find(|known| known.path_index == path)
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "test.hub",
                )
            })?;
        let disable = hub.disable_op.clone().ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.disable-op",
            )
        })?;
        central.settle_subscribe_disable(path, base + 3, out)?;
        central.report_release_success(&disable)?;
        Ok(())
    }

    /// F04 regression: rediscovering the same table keeps one current
    /// snapshot: the selector resolves without ambiguity, the old handle
    /// is stale (never ambiguous), and the live count excludes history.
    #[test]
    fn f04_rediscovery_resolves_current_without_ambiguity() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, first) = live_characteristic(&mut central, &mut out)?;
        check(
            central.resolve_path(&peer, &f04_selector())? == first,
            "first snapshot resolves",
        );
        check(central.snapshot_path_count(&peer)? == 1, "one live path");
        central.begin_discovery(&peer)?;
        central.complete_discovery(&peer)?;
        // The old handle is stale now; no current snapshot exists yet.
        expect_code(
            central.check_path_fresh(first),
            BleErrorCode::GattStaleHandle,
            BleErrorDomain::Gatt,
        )?;
        expect_code(
            central.resolve_path(&peer, &f04_selector()),
            BleErrorCode::GattNotFound,
            BleErrorDomain::Gatt,
        )?;
        // Re-registering the same table revives its slot: no duplicate.
        let second = central.register_path(
            &peer,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_WRITE | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        check(second == first, "same table revives its slot");
        check(central.paths.len() == 1, "no historical duplicate");
        check(
            central.resolve_path(&peer, &f04_selector())? == second,
            "current snapshot resolves, never ambiguous",
        );
        check(central.snapshot_path_count(&peer)? == 1, "live count exact");
        let read = central.start_read(second, 5000, 3000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let outcome = central.settle_op(&read, ContenderKind::Success, true, 9, 3001, &mut out)?;
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                check(
                    kind == OperationTerminalKind::Succeeded,
                    "current handle operates",
                );
            }
            _ => {
                check(false, "read must settle");
            }
        }
        Ok(())
    }

    /// F04 regression: 150 rediscovery cycles (past the 128-path and
    /// 32-hub defaults) with subscription churn and a mid-loop reconnect
    /// stay bounded: one live path, one live hub, current selectors keep
    /// resolving, and stale generations never match.
    #[test]
    fn f04_repeated_rediscovery_reclaims_paths_and_hubs() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, first) = live_characteristic(&mut central, &mut out)?;
        central.report_release_success(
            &central
                .op_ids
                .iter()
                .find(|id| {
                    central.operation_state(id)
                        == Some(OpStateView::Terminal(OperationTerminalKind::Succeeded))
                })
                .cloned()
                .ok_or_else(|| {
                    CoreError::new(
                        BleErrorCode::ArgumentInvalid,
                        BleErrorDomain::Core,
                        "test.connect-op",
                    )
                })?,
        )?;
        for round in 0..150u64 {
            // A fresh effect batch per round, the way a host drains executed
            // effects between calls: kernel admissions never wedge on a
            // stale full batch.
            let mut out = batch();
            if round == 75 {
                // Mid-loop reconnect: new connection generation, same table.
                central.disconnect(&peer, "client-1", 9000, &mut out)?;
                central.note_link_released(&peer)?;
                let op = central.connect(&peer, "client-1", 5000, 9001, &mut out)?;
                central.dispatch_op(&op, &mut out)?;
                central.settle_op(&op, ContenderKind::Success, true, round, 9002, &mut out)?;
                central.report_release_success(&op)?;
                central.note_link_established(&peer)?;
            }
            central.begin_discovery(&peer)?;
            central.complete_discovery(&peer)?;
            let path = central.register_path(
                &peer,
                "180D",
                0,
                Some("2A37"),
                Some(0),
                None,
                None,
                GATT_PROP_READ | GATT_PROP_WRITE | GATT_PROP_NOTIFY,
                "lease-1",
            )?;
            check(path == first, "slot revived every round");
            check(
                central.resolve_path(&peer, &f04_selector())? == first,
                "current resolves every round",
            );
            check(
                central.snapshot_path_count(&peer)? == 1,
                "live count stays one",
            );
            if round % 25 == 0 {
                f04_cycle(&mut central, &mut out, first, "app-a", 10_000 + round * 10)?;
            }
        }
        let mut out = batch();
        check(central.paths.len() == 1, "paths bounded past 128");
        check(central.hubs.len() == 1, "hubs bounded past 32");
        check(central.snapshot_path_count(&peer)? == 1, "final live count");
        let read = central.start_read(first, 5000, 20_000, &mut out)?;
        check(
            central.operation_state(&read).is_some(),
            "current handle still operates",
        );
        Ok(())
    }

    #[test]
    fn gatt02_service_change_invalidates_old_generation() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let before_ops = central.live_operation_count();
        let before_effects = out.len();
        central.services_changed(&peer)?;
        check(
            central.database_state(&peer) == Some(DatabaseState::Changed),
            "database changed",
        );
        expect_code(
            central.snapshot_path_count(&peer),
            BleErrorCode::GattStaleHandle,
            BleErrorDomain::Gatt,
        )?;
        // Stale reads reject before dispatch: no new op, no new effect.
        expect_code(
            central.start_read(path, 5000, 3000, &mut out),
            BleErrorCode::GattStaleHandle,
            BleErrorDomain::Gatt,
        )?;
        check(
            central.live_operation_count() == before_ops,
            "no op admitted",
        );
        check(out.len() == before_effects, "no dispatch");
        // Deliberate rediscovery rebinds a working generation.
        central.require_rediscovery(&peer)?;
        central.begin_discovery(&peer)?;
        central.complete_discovery(&peer)?;
        let fresh = central.register_path(
            &peer,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ,
            "lease-1",
        )?;
        let read = central.start_read(fresh, 5000, 3001, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let outcome = central.settle_op(&read, ContenderKind::Success, true, 9, 3002, &mut out)?;
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                check(
                    kind == OperationTerminalKind::Succeeded,
                    "new generation reads",
                );
            }
            _ => {
                check(false, "read must settle");
            }
        }
        Ok(())
    }

    #[test]
    fn undiscovered_read_needs_discovery() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let op = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        central.dispatch_op(&op, &mut out)?;
        central.settle_op(&op, ContenderKind::Success, true, 0, 1001, &mut out)?;
        central.note_link_established(&peer)?;
        expect_code(
            central.snapshot_path_count(&peer),
            BleErrorCode::GattDiscoveryRequired,
            BleErrorDomain::Gatt,
        )
    }

    #[test]
    fn read_write_round_trip_with_success_receipt() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let max = central.maximum_write_length(Some(512), Some(185), Some(256), "write.maximum")?;
        check(max == 185, "stacked minimum");
        let read = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let settled = central.settle_op(&read, ContenderKind::Success, true, 1, 2001, &mut out)?;
        match settled {
            CompletionOutcome::Settled {
                kind,
                cause,
                reached_radio,
                ..
            } => {
                check(kind == OperationTerminalKind::Succeeded, "read succeeded");
                check(cause.is_none(), "success carries no cause");
                check(reached_radio, "dispatched read reached radio");
            }
            _ => {
                check(false, "read must settle");
            }
        }
        let write = central.start_write(
            path,
            "with-response",
            20,
            Some(512),
            true,
            5000,
            2002,
            &mut out,
        )?;
        central.dispatch_op(&write, &mut out)?;
        let written = central.settle_op(&write, ContenderKind::Success, true, 2, 2003, &mut out)?;
        match written {
            CompletionOutcome::Settled { kind, .. } => {
                check(kind == OperationTerminalKind::Succeeded, "write succeeded");
            }
            _ => {
                check(false, "write must settle");
            }
        }
        // Zero-length values are valid payloads, never absence markers.
        let empty = central.start_write(
            path,
            "with-response",
            0,
            Some(512),
            true,
            5000,
            2004,
            &mut out,
        )?;
        check(
            central.operation_state(&empty) == Some(OpStateView::Queued),
            "empty admitted",
        );
        Ok(())
    }

    #[test]
    fn write_validation_order_matches_contract() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        expect_code(
            central.start_write(
                path,
                "eventually",
                20,
                Some(512),
                true,
                5000,
                2000,
                &mut out,
            ),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
        )?;
        expect_code(
            central.start_write(
                path,
                "without-response",
                20,
                Some(512),
                false,
                5000,
                2000,
                &mut out,
            ),
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            central.start_write(
                path,
                "with-response",
                600,
                Some(512),
                true,
                5000,
                2000,
                &mut out,
            ),
            BleErrorCode::BytesTooLarge,
            BleErrorDomain::Gatt,
        )?;
        expect_code(
            central.start_write(path, "with-response", 20, None, true, 5000, 2000, &mut out),
            BleErrorCode::CapabilityUnavailable,
            BleErrorDomain::Capability,
        )
    }

    #[test]
    fn long_write_plans_and_fails_partially() -> Result<(), CoreError> {
        let plan = plan_long_write(600, Some(512), Some(185), Some(256))?;
        check(plan.segment_maximum() == 185, "minimum wins");
        check(plan.segments() == 4, "ceil division");
        check(!plan.atomic(), "never atomic");
        expect_code(
            plan_long_write(600, None, Some(185), Some(256)),
            BleErrorCode::CapabilityUnavailable,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            plan_long_write(600, Some(0), Some(185), Some(256)),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Gatt,
        )?;
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let outcome = central.execute_long_write(
            path,
            600,
            Some(512),
            Some(185),
            Some(256),
            Some(2),
            5000,
            4000,
            &mut out,
        )?;
        check(outcome.segments() == 4, "four planned");
        check(outcome.completed() == 2, "two completed before failure");
        check(outcome.failed_at() == Some(2), "failure index kept");
        check(!outcome.is_complete(), "partial, not complete");
        let error = outcome.error("long-write.segments").ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.long-write",
            )
        })?;
        check(
            error.code() == BleErrorCode::GattWriteFailed,
            "partial failure identity",
        );
        let clean = central.execute_long_write(
            path,
            370,
            Some(512),
            Some(185),
            Some(256),
            None,
            5000,
            5000,
            &mut out,
        )?;
        check(
            clean.is_complete() && clean.completed() == 2,
            "clean run completes",
        );
        check(
            clean.error("long-write.segments").is_none(),
            "no error when complete",
        );
        Ok(())
    }

    #[test]
    fn descriptor_read_write_crosses_with_cccd_guard() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, char_path) = live_characteristic(&mut central, &mut out)?;
        let descriptor = central.register_path(
            &peer,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            Some("2902"),
            Some(0),
            GATT_PROP_READ | GATT_PROP_WRITE,
            "lease-1",
        )?;
        let read = central.start_read_descriptor(descriptor, 5000, 2000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let settled = central.settle_op(&read, ContenderKind::Success, true, 1, 2001, &mut out)?;
        match settled {
            CompletionOutcome::Settled { kind, .. } => {
                check(kind == OperationTerminalKind::Succeeded, "descriptor read");
            }
            _ => {
                check(false, "descriptor read must settle");
            }
        }
        // The CCCD is managed: direct writes must use subscribe.
        expect_code(
            central.start_write_descriptor(descriptor, 2, Some(512), 5000, 2002, &mut out),
            BleErrorCode::GattCccdManaged,
            BleErrorDomain::Gatt,
        )?;
        // Characteristic-level paths are not descriptors.
        expect_code(
            central.start_read_descriptor(char_path, 5000, 2002, &mut out),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn missing_property_fails_before_admission() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, _char) = live_characteristic(&mut central, &mut out)?;
        let read_only = central.register_path(
            &peer,
            "1800",
            0,
            Some("2A00"),
            Some(0),
            None,
            None,
            GATT_PROP_READ,
            "lease-1",
        )?;
        let before = central.live_operation_count();
        expect_code(
            central.start_write(
                read_only,
                "with-response",
                4,
                Some(512),
                true,
                5000,
                2000,
                &mut out,
            ),
            BleErrorCode::GattPropertyNotSupported,
            BleErrorDomain::Gatt,
        )?;
        check(central.live_operation_count() == before, "no op admitted");
        Ok(())
    }

    #[test]
    fn duplicate_completion_settles_once() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let first = central.start_read(path, 5000, 2000, &mut out)?;
        let second = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&first, &mut out)?;
        central.dispatch_op(&second, &mut out)?;
        let one = central.settle_op(&first, ContenderKind::Success, true, 1, 2001, &mut out)?;
        match one {
            CompletionOutcome::Settled { suppressed, .. } => {
                check(suppressed == 0, "first wins clean");
            }
            _ => {
                check(false, "first must settle");
            }
        }
        let again = central.settle_op(&first, ContenderKind::Success, true, 2, 2002, &mut out)?;
        match again {
            CompletionOutcome::DuplicateSuppressed { suppressed } => {
                check(suppressed == 1, "duplicate suppressed once");
            }
            _ => {
                check(false, "duplicate must suppress");
            }
        }
        check(
            central.suppressed_count(&first) == Some(1),
            "suppressed counted",
        );
        let two = central.settle_op(&second, ContenderKind::Success, true, 3, 2003, &mut out)?;
        match two {
            CompletionOutcome::Settled { kind, .. } => {
                check(
                    kind == OperationTerminalKind::Succeeded,
                    "second independent",
                );
            }
            _ => {
                check(false, "second must settle");
            }
        }
        // Invalid contenders never contend and change nothing.
        let third = central.start_read(path, 5000, 2004, &mut out)?;
        central.dispatch_op(&third, &mut out)?;
        let ignored =
            central.settle_op(&third, ContenderKind::Success, false, 4, 2005, &mut out)?;
        check(
            ignored == CompletionOutcome::ContenderIgnored,
            "invalid ignored",
        );
        check(
            central.operation_state(&third) == Some(OpStateView::Dispatched),
            "still live after invalid",
        );
        Ok(())
    }

    #[test]
    fn ops01_cancelled_queue_never_reaches_radio() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let write = central.start_write(
            path,
            "with-response",
            8,
            Some(512),
            true,
            5000,
            2000,
            &mut out,
        )?;
        let outcome = central.cancel_op(&write, 2001, &mut out)?;
        match outcome {
            CompletionOutcome::Settled {
                kind,
                cause,
                reached_radio,
                commit,
                ..
            } => {
                check(kind == OperationTerminalKind::Aborted, "aborted");
                check(cause == Some(BleErrorCode::OperationAborted), "abort cause");
                check(!reached_radio, "never reached radio");
                check(
                    commit == crate::contracts::CommitState::NotDispatched,
                    "not-dispatched commit",
                );
            }
            _ => {
                check(false, "cancel must settle");
            }
        }
        Ok(())
    }

    #[test]
    fn cancel_across_boundary_settles_once_and_stays_clean() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let write = central.start_write(
            path,
            "with-response",
            8,
            Some(512),
            true,
            5000,
            2000,
            &mut out,
        )?;
        let _ = central.cancel_op(&write, 2001, &mut out)?;
        // Dispatch after cancel is a state violation, not a resurrection.
        expect_code(
            central.dispatch_op(&write, &mut out),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )?;
        // A racing completion after cancel suppresses instead of settling twice.
        let late = central.settle_op(&write, ContenderKind::Success, true, 7, 2002, &mut out)?;
        match late {
            CompletionOutcome::DuplicateSuppressed { .. } => {}
            _ => {
                check(false, "late completion must suppress");
            }
        }
        // The completion boundary stays clean: fresh reads still work.
        let read = central.start_read(path, 5000, 2003, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let settled = central.settle_op(&read, ContenderKind::Success, true, 8, 2004, &mut out)?;
        match settled {
            CompletionOutcome::Settled { kind, .. } => {
                check(kind == OperationTerminalKind::Succeeded, "boundary clean");
            }
            _ => {
                check(false, "read must settle");
            }
        }
        Ok(())
    }

    #[test]
    fn ops02_timed_out_write_is_unknown_without_duplicate() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let before = central.live_operation_count();
        let write = central.start_write(
            path,
            "with-response",
            8,
            Some(512),
            true,
            5000,
            2000,
            &mut out,
        )?;
        central.dispatch_op(&write, &mut out)?;
        let outcome = central.settle_op(&write, ContenderKind::Timeout, true, 5, 2001, &mut out)?;
        match outcome {
            CompletionOutcome::Settled { kind, commit, .. } => {
                check(kind == OperationTerminalKind::TimedOut, "timed out");
                check(
                    commit == crate::contracts::CommitState::Unknown,
                    "unknown commit, no duplicate issued",
                );
            }
            _ => {
                check(false, "timeout must settle");
            }
        }
        check(
            central.live_operation_count() == before + 1,
            "no automatic duplicate write",
        );
        Ok(())
    }

    #[test]
    fn expire_sweep_settles_queued_timeouts() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _read = central.start_read(path, 50, 2000, &mut out)?;
        let (settled, truncated) = central.expire_sweep(2100, &mut out)?;
        check(settled == 1, "one expiry");
        check(!truncated, "no truncation");
        Ok(())
    }

    #[test]
    fn capability_truth_gates_and_parity_survives() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        central.register_capability(CapabilityDescriptor::new(
            "central.read",
            CapabilityState::Supported,
            &[("max-bytes", 512)],
            &[],
            "receipt-1",
            EvidenceLevel::Deterministic,
            "ubm-core-0.1.0",
            "digest-1",
            &["scenario.scan-connect-discover-read-notify-destroy"],
        )?)?;
        central.register_capability(CapabilityDescriptor::new(
            "central.write",
            CapabilityState::Limited,
            &[("max-bytes", 20)],
            &["write-without-response-unpaced"],
            "receipt-2",
            EvidenceLevel::Deterministic,
            "ubm-core-0.1.0",
            "digest-2",
            &["scenario.scan-connect-discover-read-notify-destroy"],
        )?)?;
        check(
            central.check_capability("central.read", "read")? == CapabilityAdmission::Proceed,
            "supported proceeds",
        );
        check(
            central.check_capability("central.write", "write")?
                == CapabilityAdmission::ProceedWithLimitation,
            "limited proceeds explicitly",
        );
        expect_code(
            central.check_capability("central.subscribe", "subscribe"),
            BleErrorCode::CapabilityUnavailable,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            central.check_capability("central.unknown-thing", "x"),
            BleErrorCode::CapabilityUnavailable,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            CapabilityDescriptor::new(
                "central.scan",
                CapabilityState::Unsupported,
                &[],
                &[],
                "receipt-3",
                EvidenceLevel::Blocked,
                "ubm-core-0.1.0",
                "digest-3",
                &[],
            ),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Capability,
        )?;
        // Unsupported states cannot erase required parity rows.
        central.register_capability(CapabilityDescriptor::new(
            "central.subscribe",
            CapabilityState::Unsupported,
            &[],
            &["host-has-no-ble-radio"],
            "receipt-4",
            EvidenceLevel::Blocked,
            "ubm-core-0.1.0",
            "digest-4",
            &[],
        )?)?;
        let rows = central.parity_rows();
        check(
            rows.len() == REQUIRED_CAPABILITY_IDS.len(),
            "rows never erased",
        );
        expect_code(
            central.check_capability("central.subscribe", "subscribe"),
            BleErrorCode::CapabilityUnsupported,
            BleErrorDomain::Capability,
        )
    }

    #[test]
    fn controls_are_typed_acceptance_without_proof() -> Result<(), CoreError> {
        check(CENTRAL_CONTROLS.len() == 10, "frozen control count");
        for control in CENTRAL_CONTROLS.iter() {
            check(!control.acceptance_is_proof, "acceptance is never proof");
        }
        let mtu = central_control_for("requestMtu").ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.control",
            )
        })?;
        check(
            mtu.capability_id == "connection:request-mtu",
            "control binding",
        );
        check(
            central_control_for("launchMissiles").is_none(),
            "unknown control",
        );
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, _path) = live_characteristic(&mut central, &mut out)?;
        central.register_capability(CapabilityDescriptor::new(
            "connection:request-mtu",
            CapabilityState::Supported,
            &[],
            &[],
            "receipt-9",
            EvidenceLevel::Deterministic,
            "ubm-core-0.1.0",
            "digest-9",
            &[],
        )?)?;
        let (op, proves) =
            central.start_control("requestMtu", Some(&peer), 5000, 2000, &mut out)?;
        check(!proves, "request is not observation");
        check(
            central.operation_state(&op) == Some(OpStateView::Queued),
            "control admitted",
        );
        expect_code(
            central.start_control("requestPhy", Some(&peer), 5000, 2000, &mut out),
            BleErrorCode::CapabilityUnavailable,
            BleErrorDomain::Capability,
        )?;
        expect_code(
            central.start_control("teleport", Some(&peer), 5000, 2000, &mut out),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn handshake_gate_blocks_admission() -> Result<(), CoreError> {
        let mut central = Central::new_with_handshake(
            fixture_attachment()?,
            Generation::new("kernel-gen-1")?,
            CentralConfig::default(),
            HandshakeState { complete: false },
        )?;
        let mut out = batch();
        let request = validate_scan_request(&[], "all", "none", 5000, false, &[])?;
        expect_code(
            central.start_scan(&request, None, "owner-a", 1000, &mut out),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn security_ops_fail_closed_without_availability() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, _path) = live_characteristic(&mut central, &mut out)?;
        expect_code(
            central.start_security("pair", &peer, 5000, 2000, &mut out),
            BleErrorCode::PlatformSecurity,
            BleErrorDomain::Platform,
        )?;
        central.set_security_available(true);
        let op = central.start_security("pair", &peer, 5000, 2000, &mut out)?;
        check(
            central.operation_state(&op) == Some(OpStateView::Queued),
            "pair admitted",
        );
        let outcome = central.settle_security(&peer, true)?;
        check(outcome == "paired", "pair settles");
        // A won bond stays won: cancel reports paired, consistent with pair.
        let cancelled = central.cancel_pairing(&peer)?;
        check(cancelled == "paired", "cancel consistent with pair");
        expect_code(
            central.start_security("hypnotize", &peer, 5000, 2000, &mut out),
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.start_security("pair", "public-address:ghost", 5000, 2000, &mut out),
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
        )
    }

    #[test]
    fn gatt03_shared_enablement_survives_single_removal() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _first =
            central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        check(!central.physical_cccd_enabled(path), "enable pending");
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        check(central.physical_cccd_enabled(path), "physical enabled once");
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Ready),
            "first ready",
        );
        let _second =
            central.subscribe(path, "drop-oldest", 4, 128, "app-b", 5000, 2002, &mut out)?;
        check(
            central.consumer_state(path, "app-b") == Some(ConsumerState::Ready),
            "borrower joins live enablement",
        );
        // Removing one consumer never drops the other's CCCD.
        let disabled = central.unsubscribe(path, "app-a", 2003, &mut out)?;
        check(!disabled, "no physical disable while app-b lives");
        check(central.physical_cccd_enabled(path), "CCCD stays enabled");
        check(
            central.consumer_state(path, "app-b") == Some(ConsumerState::Ready),
            "app-b undisturbed",
        );
        // The last removal disables.
        let last = central.unsubscribe(path, "app-b", 2004, &mut out)?;
        check(last, "last removal disables");
        central.settle_subscribe_disable(path, 2005, &mut out)?;
        check(!central.physical_cccd_enabled(path), "physical disabled");
        // Repeat removal is idempotent, never an error.
        let again = central.unsubscribe(path, "app-b", 2006, &mut out)?;
        check(!again, "idempotent removal");
        Ok(())
    }

    #[test]
    fn remove_during_enable_reconciles_late_completion() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _first =
            central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        let disabled = central.unsubscribe(path, "app-a", 2001, &mut out)?;
        check(!disabled, "no disable while enabling");
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removing),
            "remove-during-enable parks in removing",
        );
        // Late native success with no consumers left disables immediately:
        // no orphan live stream (CLN-03).
        central.settle_subscribe_enable(path, true, 2002, &mut out)?;
        check(
            !central.physical_cccd_enabled(path),
            "reconciled, not orphaned",
        );
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removed),
            "consumer removed",
        );
        Ok(())
    }

    /// F12 regression: cancelling the only subscribe operation during
    /// enable leaves no waiters: the late native success revives nothing,
    /// delivers nothing, and stages exactly one compensating physical
    /// disable instead of silently reporting disabled.
    #[test]
    fn f12_cancelled_subscriber_never_revives() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.cancel_op(&sub, 2001, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removed),
            "cancel leaves the enable wait set",
        );
        let _ = central.drain_typed_effects();
        central.settle_subscribe_enable(path, true, 2002, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removed),
            "late success revives nothing",
        );
        check(
            !central.physical_cccd_enabled(path),
            "no orphan live stream",
        );
        let staged = central.drain_typed_effects();
        let disables = staged
            .iter()
            .filter(|effect| effect.kind() == CentralEffectKind::SubscribeDisable)
            .count();
        check(disables == 1, "exactly one compensating disable");
        // The hub reports disabled only after the OS confirms the cleanup.
        central.settle_subscribe_disable(path, 2003, &mut out)?;
        let late = central.deliver_notification(path, 1)?;
        check(
            late[0].1 == DeliveryOutcome::DroppedRemoved,
            "cancelled consumer receives nothing",
        );
        Ok(())
    }

    /// F12 regression: removing the last subscriber during enable, then
    /// completing the native enable, stages a real compensating disable
    /// (the CCCD is live with no owner) and finalizes the consumer only
    /// after the OS confirms.
    #[test]
    fn f12_last_unsubscribe_during_enable_stages_real_disable() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        let disabled = central.unsubscribe(path, "app-a", 2001, &mut out)?;
        check(!disabled, "no disable while enabling");
        let _ = central.drain_typed_effects();
        central.settle_subscribe_enable(path, true, 2002, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removed),
            "consumer finalized",
        );
        let staged = central.drain_typed_effects();
        let disables = staged
            .iter()
            .filter(|effect| effect.kind() == CentralEffectKind::SubscribeDisable)
            .count();
        check(disables == 1, "exactly one compensating disable");
        central.settle_subscribe_disable(path, 2003, &mut out)?;
        check(
            !central.physical_cccd_enabled(path),
            "cleanup confirmed disabled",
        );
        Ok(())
    }

    #[test]
    fn gatt04_pre_ready_values_quarantine_never_deliver() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        let early = central.deliver_notification(path, 1)?;
        check(early.len() == 1, "one consumer observed");
        check(
            early[0].1 == DeliveryOutcome::QuarantinedPreReady,
            "pre-ready quarantined",
        );
        check(
            central.quarantined_count(path, "app-a") == Some(1),
            "quarantine counted",
        );
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        let live = central.deliver_notification(path, 1)?;
        check(
            live[0].1 == DeliveryOutcome::Delivered,
            "ready values deliver",
        );
        check(
            central.quarantined_count(path, "app-a") == Some(1),
            "no retro-delivery",
        );
        Ok(())
    }

    #[test]
    fn str02_subscription_overflow_is_bounded_and_terminal() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        // Contract-valid budgets: item cap 1, bytes above the reserved 64.
        let _sub = central.subscribe(path, "error", 1, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        let first = central.deliver_notification(path, 1)?;
        check(first[0].1 == DeliveryOutcome::Delivered, "first admitted");
        // Under the `error` policy the rejecting value still reports
        // `Delivered` while raising the once-only overflow terminal.
        let second = central.deliver_notification(path, 1)?;
        check(
            second[0].1 == DeliveryOutcome::Delivered,
            "rejecting value reports delivered with terminal",
        );
        let terminal = central.take_terminal(path, "app-a");
        match terminal {
            Some(event) => {
                check(event.reason() == "overflow", "overflow reason");
                check(event.dropped_items() == 1, "one dropped item");
                check(event.dropped_bytes() == 1, "one dropped byte");
                check(event.replaced_items() == 0, "nothing replaced");
            }
            None => {
                check(false, "exactly one terminal expected");
            }
        }
        check(
            central.take_terminal(path, "app-a").is_none(),
            "terminal occurs once",
        );
        // After the terminal, late values cannot reach the old consumer.
        let late = central.deliver_notification(path, 1)?;
        check(late[0].1 == DeliveryOutcome::DroppedLate, "no late value");
        // Contract budgets below the reserved control bytes fail closed.
        expect_code(
            central.subscribe(path, "error", 1, 8, "app-b", 5000, 2002, &mut out),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )
    }

    #[test]
    fn service_change_invalidates_subscriptions() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.services_changed(&peer)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Invalid),
            "consumers invalidated",
        );
        let late = central.deliver_notification(path, 1)?;
        check(
            late[0].1 == DeliveryOutcome::DroppedLate,
            "no delivery after change",
        );
        Ok(())
    }

    #[test]
    fn cln02_link_loss_races_disconnect_exactly_once() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, _path) = live_characteristic(&mut central, &mut out)?;
        central.disconnect(&peer, "client-1", 2000, &mut out)?;
        let terminal = central.note_peer_loss(&peer, 2001, &mut out)?;
        check(terminal == ConnectionState::Lost, "one coherent terminal");
        // No resurrection and no second release.
        expect_code(
            central.note_peer_loss(&peer, 2002, &mut out),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.note_link_released(&peer),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )?;
        check(
            central.connection_state(&peer) == Some(ConnectionState::Lost),
            "terminal holds",
        );
        Ok(())
    }

    #[test]
    fn cln01_failed_disconnect_retained_and_reported() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let read = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        central.settle_op(&read, ContenderKind::Success, true, 1, 2001, &mut out)?;
        central.report_release_failure(&read, BleErrorCode::PlatformTransport)?;
        check(central.retained_cleanup_count() == 1, "failure retained");
        let retained = central.drain_cleanup(8);
        check(retained.len() == 1, "failure reported");
        check(
            retained[0].failures().len() == 1
                && retained[0].failures()[0].code() == BleErrorCode::PlatformTransport,
            "failure identity kept",
        );
        // Disconnect failure is retained on the connection and surfaces at destroy.
        central.report_disconnect_failure(&peer, BleErrorCode::ConnectionLost)?;
        let record = central.destroy(&mut out)?;
        check(
            record.state() == crate::ownership::CleanupState::ReleaseFailed,
            "destroy reports retained failure",
        );
        check(!record.failures().is_empty(), "failures visible");
        // Duplicate destroy returns the same authoritative record.
        let again = central.destroy(&mut out)?;
        check(again == record, "idempotent destroy");
        expect_code(
            central.start_read(path, 5000, 2002, &mut out),
            BleErrorCode::LifecycleDestroyed,
            BleErrorDomain::Core,
        )
    }

    #[test]
    fn ops03_contention_bounds_hold_and_cleanup_flows() -> Result<(), CoreError> {
        let config = CentralConfig::new(16, 16, 128, 32, 8, 256, KernelConfig::new(4, 64, 2)?)?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let request = validate_scan_request(&[], "all", "none", 5000, false, &[])?;
        let _one = central.start_scan(&request, Some("a"), "owner-x", 1000, &mut out)?;
        let _two = central.start_scan(&request, Some("b"), "owner-x", 1001, &mut out)?;
        // Per-owner bound trips before the aggregate does.
        expect_code(
            central.start_scan(&request, Some("c"), "owner-x", 1002, &mut out),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        // Another owner still fits until the aggregate bound trips.
        let _three = central.start_scan(&request, Some("d"), "owner-y", 1003, &mut out)?;
        let _four = central.start_scan(&request, Some("e"), "owner-y", 1004, &mut out)?;
        expect_code(
            central.start_scan(&request, Some("f"), "owner-z", 1005, &mut out),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        // Cleanup is not starved by contention.
        let drained = central.drain_cleanup(8);
        check(drained.is_empty(), "nothing retained yet");
        Ok(())
    }

    #[test]
    fn ops04_shutdown_terminates_owned_work_bounded() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let queued = central.start_read(path, 5000, 2000, &mut out)?;
        let flying = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&flying, &mut out)?;
        let record = central.destroy(&mut out)?;
        check(
            record.state() == crate::ownership::CleanupState::Released,
            "clean shutdown",
        );
        check(
            central.operation_state(&queued)
                == Some(OpStateView::Terminal(OperationTerminalKind::Destroyed)),
            "queued destroyed",
        );
        Ok(())
    }

    #[test]
    fn str01_control_survives_data_overflow() -> Result<(), CoreError> {
        let limits = StreamLimits::new(1, 128, 1, 64)?;
        let mut stream = Stream::new(limits, OverflowPolicy::DropOldest);
        let first = stream.push_data(64)?;
        check(
            first.decision == crate::streams::AdmissionDecision::Admit,
            "first admitted",
        );
        let overflow = stream.push_data(64)?;
        check(
            overflow.decision == crate::streams::AdmissionDecision::DropOldest,
            "drop-oldest stays active",
        );
        // Reserved control bypasses a full data queue on the shared budget.
        let control = stream.push_control(8)?;
        check(
            control.decision == crate::streams::AdmissionDecision::Admit,
            "control delivered",
        );
        let latest = Stream::new(limits, OverflowPolicy::Latest);
        check(latest.policy() == OverflowPolicy::Latest, "policy kept");
        Ok(())
    }

    #[test]
    fn own05_borrower_exit_keeps_restoration() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        check(central.restoration_authority(), "authority starts owned");
        central.set_sharing_supported(true);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let _second = central.borrow_connection(&peer, "borrower-9", 5000, 1001, &mut out)?;
        central.release_borrower(&peer, "borrower-9")?;
        check(
            central.connection_lease_count(&peer) == 1,
            "borrower released",
        );
        check(central.restoration_authority(), "restoration survives");
        Ok(())
    }

    #[test]
    fn vectors_and_corrections_cross_check() -> Result<(), CoreError> {
        check(RACE_BOUNDS_CLEANUP_VECTOR_IDS.len() == 7, "seven vectors");
        for vector in RACE_BOUNDS_CLEANUP_VECTOR_IDS.iter() {
            check(!vector.is_empty(), "vector named");
        }
        check(
            APPROVED_CORRECTION_CANDIDATES.len() == 3,
            "three candidates",
        );
        for candidate in APPROVED_CORRECTION_CANDIDATES.iter() {
            check(!candidate.vector_id.is_empty(), "candidate vector");
            check(!candidate.observed.is_empty(), "observation logged");
            check(!candidate.resolution.is_empty(), "resolution logged");
            let mut known = false;
            for vector in RACE_BOUNDS_CLEANUP_VECTOR_IDS.iter() {
                if *vector == candidate.vector_id {
                    known = true;
                }
            }
            check(known, "candidate names a served vector");
            check(
                correction_for(candidate.vector_id).is_some(),
                "candidate retrievable",
            );
        }
        check(
            correction_for("no.such-vector").is_none(),
            "unknown vector has no candidate",
        );
        Ok(())
    }

    #[test]
    fn m2_unsubscribe_ready_goes_through_removing() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Ready),
            "ready before remove",
        );
        let disabled = central.unsubscribe(path, "app-a", 2002, &mut out)?;
        check(disabled, "last removal issues disable");
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removing),
            "ready remove goes to removing",
        );
        let delivery = central.deliver_notification(path, 1)?;
        check(
            delivery[0].1 == DeliveryOutcome::Delivered,
            "delivery during removing still delivers",
        );
        central.settle_subscribe_disable(path, 2003, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Removed),
            "cccd-disabled completes to removed",
        );
        Ok(())
    }

    /// F16 regression: sixteen unrelated advertisers with zero active
    /// connections must not lock out a target sensor. Unreferenced
    /// discoveries evict oldest-first; the cache stays bounded and the
    /// target remains discoverable and connectable.
    #[test]
    fn f16_discovery_pressure_keeps_target_admissible() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let mut oldest = String::new();
        for index in 0..16u32 {
            let value = format!("AA:BB:CC:DD:EE:{index:02X}");
            let key = central.resolve_peer("public-address", &value)?;
            if index == 0 {
                oldest = key;
            }
        }
        check(central.known_peer_count() == 16, "cache full, zero links");
        let target = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:FF")?;
        check(central.known_peer_count() == 16, "cache stays bounded");
        central.require_known_peer(&target)?;
        expect_code(
            central.require_known_peer(&oldest),
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
        )?;
        // The target is connectable without a new manager.
        let _op = central.connect(&target, "client-1", 5000, 1000, &mut out)?;
        check(
            central.connection_state(&target) == Some(ConnectionState::Connecting),
            "target connects under pressure",
        );
        Ok(())
    }

    /// F16 regression: connected (pinned) peers survive discovery
    /// pressure; eviction takes an unreferenced discovery instead.
    #[test]
    fn f16_connected_peers_pin_against_eviction() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let pinned = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _op = central.connect(&pinned, "client-1", 5000, 1000, &mut out)?;
        for index in 2..17u32 {
            let value = format!("AA:BB:CC:DD:EE:{index:02X}");
            central.resolve_peer("public-address", &value)?;
        }
        check(central.known_peer_count() == 16, "cache full, one link");
        let newcomer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:FF")?;
        central.require_known_peer(&pinned)?;
        central.require_known_peer(&newcomer)?;
        check(central.known_peer_count() == 16, "bounded with pinning");
        Ok(())
    }

    /// F16 regression: when every cached discovery is pinned, the next
    /// discovery fails closed with an explicit quota instead of silently
    /// losing the peer.
    #[test]
    fn f16_full_pinned_cache_reports_quota() -> Result<(), CoreError> {
        let config = CentralConfig::new(1, 1, 128, 32, 8, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let pinned = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _op = central.connect(&pinned, "client-1", 5000, 1000, &mut out)?;
        expect_code(
            central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02"),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        central.require_known_peer(&pinned)?;
        Ok(())
    }

    #[test]
    fn l1_peer_and_connection_bounds_reject_directly() -> Result<(), CoreError> {
        // F16 contract change (justified update): the old assertion rejected
        // a second discovery with zero connections, encoding the bug where
        // the discovery cache shared the connection bound without eviction.
        // Unreferenced discoveries now evict; rejection is direct only when
        // every cached discovery is pinned (here: connected).
        let config = CentralConfig::new(1, 1, 128, 32, 8, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let a = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _ca = central.connect(&a, "client-1", 5000, 1000, &mut out)?;
        expect_code(
            central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02"),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        // F19 contract change (justified update): the old step rekeyed B
        // onto A while both held live links, encoding the unsafe silent
        // merge (B's peer record dropped while its live link kept the old
        // key). That merge now fails explicitly; the connection bound is
        // tested directly with three distinct peers.
        let config = CentralConfig::new(2, 4, 128, 32, 8, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let a = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let b = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02")?;
        let c = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:03")?;
        let _ca = central.connect(&a, "client-1", 5000, 1000, &mut out)?;
        let _cb = central.connect(&b, "client-2", 5000, 1001, &mut out)?;
        expect_code(
            central.connect(&c, "client-3", 5000, 1002, &mut out),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        // The discovery cache is independent of the connection bound: with
        // both links full, discoveries still admit.
        let _d = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:04")?;
        Ok(())
    }

    #[test]
    fn l1_path_bound_rejects_directly() -> Result<(), CoreError> {
        let config = CentralConfig::new(16, 16, 2, 32, 8, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let (peer, _first) = live_characteristic(&mut central, &mut out)?;
        let _second = central.register_path(
            &peer,
            "180D",
            1,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ,
            "lease-1",
        )?;
        expect_code(
            central.register_path(
                &peer,
                "180D",
                2,
                Some("2A37"),
                Some(0),
                None,
                None,
                GATT_PROP_READ,
                "lease-1",
            ),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        Ok(())
    }

    #[test]
    fn l1_subscription_hub_and_consumer_bounds_reject_directly() -> Result<(), CoreError> {
        let config = CentralConfig::new(16, 16, 8, 1, 8, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let (peer, first) = live_characteristic(&mut central, &mut out)?;
        let second = central.register_path(
            &peer,
            "180D",
            1,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        let _sub =
            central.subscribe(first, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        expect_code(
            central.subscribe(second, "drop-oldest", 4, 128, "app-b", 5000, 2001, &mut out),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        let config = CentralConfig::new(16, 16, 128, 32, 1, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        expect_code(
            central.subscribe(path, "drop-oldest", 4, 128, "app-b", 5000, 2002, &mut out),
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
        )?;
        Ok(())
    }

    #[test]
    fn l1_unbounded_tables_do_not_wedge_admission() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let request = validate_scan_request(&[], "all", "none", 5000, false, &[])?;
        for round in 0..3u64 {
            let base = 4000 + round * 10;
            let scan = central.start_scan(&request, None, "owner-x", base, &mut out)?;
            central.platform_scan_started(&scan)?;
            central.stop_scan(&scan, base + 1, &mut out)?;
            let terminal = central.note_scan_platform(
                &scan,
                ScanPlatformEvent::PlatformStopped,
                base + 2,
                &mut out,
            )?;
            check(
                terminal == ScanSessionState::Stopped,
                "scan reaches terminal",
            );
        }
        for index in 0..5u64 {
            let id = format!("central.extra-{index}");
            central.register_capability(CapabilityDescriptor::new(
                &id,
                CapabilityState::Supported,
                &[("max-bytes", 512)],
                &[],
                "receipt-x",
                EvidenceLevel::Deterministic,
                "ubm-core-0.1.0",
                "digest-x",
                &["scenario.scan-connect-discover-read-notify-destroy"],
            )?)?;
        }
        central.set_security_available(true);
        let _pair = central.start_security("pair", &peer, 5000, 5000, &mut out)?;
        check(
            central.settle_security(&peer, true)? == "paired",
            "pair settles",
        );
        central.report_disconnect_failure(&peer, BleErrorCode::ConnectionLost)?;
        central.report_disconnect_failure(&peer, BleErrorCode::ConnectionLost)?;
        let read = central.start_read(path, 5000, 6000, &mut out)?;
        central.dispatch_op(&read, &mut out)?;
        let outcome = central.settle_op(&read, ContenderKind::Success, true, 99, 6001, &mut out)?;
        match outcome {
            CompletionOutcome::Settled { kind, .. } => {
                check(
                    kind == OperationTerminalKind::Succeeded,
                    "admission not wedged",
                );
            }
            _ => {
                check(false, "read must settle");
            }
        }
        Ok(())
    }

    #[test]
    fn l3_empty_generation_denied_and_epoch_bounded() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_sharing_supported(true);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let _first = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        let generation = central.connection_generation(&peer).ok_or_else(|| {
            CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "test.generation",
            )
        })?;
        expect_code(
            central.transfer_lease(&peer, "client-1", "client-9", "", 1),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        expect_code(
            central.transfer_lease(
                &peer,
                "client-1",
                "client-9",
                &generation,
                9_007_199_254_740_992,
            ),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        Ok(())
    }

    /// F19 regression: a connected provisional identity resolving to an
    /// already-discovered canonical identity migrates every live reference
    /// (connection, operations, paths, subscriptions, security) onto the
    /// survivor; nothing dangles on the removed key.
    #[test]
    fn f19_collision_migrates_live_references() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let a = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let conn_a = central.connect(&a, "client-1", 5000, 1000, &mut out)?;
        central.dispatch_op(&conn_a, &mut out)?;
        central.settle_op(&conn_a, ContenderKind::Success, true, 0, 1001, &mut out)?;
        central.note_link_established(&a)?;
        central.begin_discovery(&a)?;
        central.complete_discovery(&a)?;
        let path = central.register_path(
            &a,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 1002, &mut out)?;
        central.settle_subscribe_enable(path, true, 1003, &mut out)?;
        central.set_security_available(true);
        let _pair = central.start_security("pair", &a, 5000, 1004, &mut out)?;
        let b = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02")?;
        let merged = central.update_peer_canonical(&a, "public-address", "AA:BB:CC:DD:EE:02")?;
        check(merged == b, "survivor is the canonical key");
        central.require_known_peer(&b)?;
        expect_code(
            central.require_known_peer(&a),
            BleErrorCode::PeerNotFound,
            BleErrorDomain::Connection,
        )?;
        // Connection, operations, paths, subscriptions, security agree on B.
        check(
            central.connection_state(&b) == Some(ConnectionState::Connected),
            "link follows the survivor",
        );
        check(
            central.connection_state(&a).is_none(),
            "nothing dangles on the removed key",
        );
        check(
            central.op_peers.iter().all(|(_, peer)| peer != &a),
            "no operation names the removed key",
        );
        check(
            central
                .op_peers
                .iter()
                .any(|(op, peer)| op == &conn_a && peer == &b),
            "connect op migrated",
        );
        central.check_path_fresh(path)?;
        let delivery = central.deliver_notification(path, 1)?;
        check(
            delivery[0].1 == DeliveryOutcome::Delivered,
            "subscription follows the survivor",
        );
        check(
            central.settle_security(&b, true)? == "paired",
            "exchange follows the survivor",
        );
        central.disconnect(&b, "client-1", 1005, &mut out)?;
        check(
            central.connection_state(&b) == Some(ConnectionState::Disconnecting),
            "disconnect works through the survivor",
        );
        Ok(())
    }

    /// F19 regression: when both identities hold live links, no safe merge
    /// exists: the rekey fails explicitly and every record stays untouched.
    #[test]
    fn f19_collision_with_two_live_links_rejects() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let a = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let _ca = central.connect(&a, "client-1", 5000, 1000, &mut out)?;
        let b = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02")?;
        let _cb = central.connect(&b, "client-2", 5000, 1001, &mut out)?;
        expect_code(
            central.update_peer_canonical(&a, "public-address", "AA:BB:CC:DD:EE:02"),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        // Transactional veto: both identities and both links survive intact.
        central.require_known_peer(&a)?;
        central.require_known_peer(&b)?;
        check(
            central.connection_state(&a) == Some(ConnectionState::Connecting),
            "A link untouched",
        );
        check(
            central.connection_state(&b) == Some(ConnectionState::Connecting),
            "B link untouched",
        );
        check(central.known_peer_count() == 2, "no peer lost");
        Ok(())
    }

    /// F19 regression: a live link wins over a terminal record: the dead
    /// record drops and the live one rekeys onto the survivor.
    #[test]
    fn f19_collision_prefers_live_link_over_terminal() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let a = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let _ca = central.connect(&a, "client-1", 5000, 1000, &mut out)?;
        let b = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02")?;
        let _cb = central.connect(&b, "client-2", 5000, 1001, &mut out)?;
        central.disconnect(&b, "client-2", 1002, &mut out)?;
        central.note_link_released(&b)?;
        check(
            central.connection_state(&b) == Some(ConnectionState::Disconnected),
            "B terminal before merge",
        );
        let merged = central.update_peer_canonical(&a, "public-address", "AA:BB:CC:DD:EE:02")?;
        check(merged == b, "survivor is the canonical key");
        check(
            central.connection_state(&b) == Some(ConnectionState::Connecting),
            "live link rekeyed onto the survivor",
        );
        check(
            central.connection_state(&a).is_none(),
            "terminal record dropped, nothing dangles",
        );
        Ok(())
    }

    /// F19 regression: a live pairing exchange cannot merge into an
    /// existing exchange: the rekey fails explicitly and both exchanges
    /// stay usable under their own keys.
    #[test]
    fn f19_collision_with_live_security_on_both_rejects() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_security_available(true);
        let a = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let _pair_a = central.start_security("pair", &a, 5000, 1000, &mut out)?;
        let b = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:02")?;
        let _pair_b = central.start_security("pair", &b, 5000, 1001, &mut out)?;
        check(
            central.settle_security(&b, true)? == "paired",
            "B bond won before merge",
        );
        expect_code(
            central.update_peer_canonical(&a, "public-address", "AA:BB:CC:DD:EE:02"),
            BleErrorCode::OwnershipDenied,
            BleErrorDomain::Core,
        )?;
        check(
            central.settle_security(&a, true)? == "paired",
            "A exchange untouched",
        );
        check(central.cancel_pairing(&b)? == "paired", "B bond untouched");
        Ok(())
    }

    #[test]
    fn l2_rekey_moves_security_exchange() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        central.set_security_available(true);
        let old = central.resolve_peer("resolvable-private-address", "rotating-1")?;
        let _op = central.start_security("pair", &old, 5000, 1000, &mut out)?;
        let new = central.update_peer_canonical(&old, "public-address", "AA:BB:CC:DD:EE:02")?;
        let outcome = central.settle_security(&new, true)?;
        check(
            outcome == "paired",
            "exchange survives re-key under new key",
        );
        expect_code(
            central.settle_security(&old, true),
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
        )?;
        Ok(())
    }

    #[test]
    fn m2_disconnect_and_link_release_invalidate_hubs() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.disconnect(&peer, "client-1", 2002, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Invalid),
            "disconnect invalidates consumer",
        );
        check(
            !central.physical_cccd_enabled(path),
            "hub not enabled after disconnect",
        );
        let straggler = central.deliver_notification(path, 1)?;
        check(
            straggler[0].1 != DeliveryOutcome::Delivered,
            "no delivered straggler after disconnect",
        );
        central.note_link_released(&peer)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Invalid),
            "link release keeps consumer invalid",
        );
        Ok(())
    }

    #[test]
    fn m2_peer_loss_invalidates_hubs_and_blocks_delivery() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 4, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Ready),
            "ready before loss",
        );
        check(
            central.physical_cccd_enabled(path),
            "cccd enabled before loss",
        );
        central.note_peer_loss(&peer, 2002, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Invalid),
            "peer loss invalidates consumer",
        );
        check(
            !central.physical_cccd_enabled(path),
            "hub not enabled after loss",
        );
        let straggler = central.deliver_notification(path, 1)?;
        check(
            straggler[0].1 != DeliveryOutcome::Delivered,
            "straggler delivery must not report delivered",
        );
        Ok(())
    }

    #[test]
    fn m1_success_release_reclaims_aggregate_admission() -> Result<(), CoreError> {
        let config = CentralConfig::new(16, 16, 128, 32, 8, 256, KernelConfig::new(4, 64, 8)?)?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        for index in 0..6u64 {
            let read = central.start_read(path, 5000, 2000 + index, &mut out)?;
            central.dispatch_op(&read, &mut out)?;
            central.settle_op(
                &read,
                ContenderKind::Success,
                true,
                index,
                2001 + index,
                &mut out,
            )?;
            central.report_release_success(&read)?;
        }
        let again = central.start_read(path, 5000, 3000, &mut out)?;
        check(
            central.operation_state(&again).is_some(),
            "admission still succeeds after success-release",
        );
        Ok(())
    }

    #[test]
    fn m2_notification_values_buffer_fifo_and_recycle() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "error", 4, 256, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.deliver_notification_value(path, &[0x01])?;
        central.deliver_notification_value(path, &[0x02, 0x03])?;
        check(
            central.pending_value_count(path, "app-a") == Some(2),
            "two values buffered",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x01]),
            "fifo order",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x02, 0x03]),
            "second value",
        );
        check(
            central.take_notification_value(path, "app-a").is_none(),
            "drained exactly",
        );
        // Popping freed stream bytes: delivery works again after drain.
        central.deliver_notification_value(path, &[0x04])?;
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x04]),
            "bound recycled",
        );
        Ok(())
    }

    /// F06 helper: white-box snapshot of one consumer's stream ledger
    /// `(data_items, bytes)` alongside its buffered value count.
    fn f06_ledger(
        central: &Central,
        path: usize,
        lease: &str,
    ) -> Result<(u64, u64, usize), CoreError> {
        let hub = central
            .hubs
            .iter()
            .find(|known| known.path_index == path)
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "test.hub",
                )
            })?;
        let record = hub
            .consumers
            .iter()
            .find(|known| known.lease == lease)
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::ArgumentInvalid,
                    BleErrorDomain::Core,
                    "test.consumer",
                )
            })?;
        Ok((
            record.stream.data_items(),
            record.stream.bytes(),
            record.slots.len(),
        ))
    }

    /// F06 regression: under a retaining policy the consumer receives the
    /// newest bytes, not stale ones the ledger already evicted. Exact byte
    /// sequences AND ledger sizes are asserted after every replacement,
    /// including multi-item eviction and interleaved length-only delivery.
    #[test]
    fn f06_replacement_retains_newest_bytes() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "drop-oldest", 2, 256, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        let first = central.deliver_notification_value(path, &[0x0a])?;
        check(first[0].1 == DeliveryOutcome::Delivered, "A admitted");
        let second = central.deliver_notification_value(path, &[0x0b, 0x0c])?;
        check(second[0].1 == DeliveryOutcome::Delivered, "B admitted");
        let (items, bytes, slots) = f06_ledger(&central, path, "app-a")?;
        check(items == 2 && bytes == 3 && slots == 2, "A+B ledger exact");
        // C replaces A: the ledger holds B+C and so does the value queue.
        let third = central.deliver_notification_value(path, &[0x0d, 0x0e, 0x0f])?;
        check(
            third[0].1 == DeliveryOutcome::OverflowNoticed,
            "C replaces with notice",
        );
        let (items, bytes, slots) = f06_ledger(&central, path, "app-a")?;
        check(items == 2 && bytes == 5 && slots == 2, "B+C ledger exact");
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x0b, 0x0c]),
            "oldest take is B, not stale A",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x0d, 0x0e, 0x0f]),
            "newest take is C",
        );
        check(
            central.take_notification_value(path, "app-a").is_none(),
            "drained exactly",
        );
        let (items, bytes, slots) = f06_ledger(&central, path, "app-a")?;
        check(items == 0 && bytes == 0 && slots == 0, "drain recycles all");
        // Repeated replacements keep serving the newest in FIFO order.
        central.deliver_notification_value(path, &[0x01])?;
        central.deliver_notification_value(path, &[0x02])?;
        central.deliver_notification_value(path, &[0x03])?;
        central.deliver_notification_value(path, &[0x04])?;
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x03]),
            "repeated replace keeps newest window",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x04]),
            "repeated replace newest last",
        );
        // Length-only delivery occupies the ledger without a value slot;
        // the next take still pairs the right value with the right size.
        central.deliver_notification(path, 7)?;
        central.deliver_notification_value(path, &[0x05, 0x06])?;
        check(
            central.pending_value_count(path, "app-a") == Some(1),
            "one value buffered",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x05, 0x06]),
            "interleaved take pairs value with its size",
        );
        let (items, bytes, slots) = f06_ledger(&central, path, "app-a")?;
        check(
            items == 0 && bytes == 0 && slots == 0,
            "interleave drains clean",
        );
        Ok(())
    }

    /// F06 regression: multi-item eviction drops the same payloads from the
    /// value queue that the ledger evicts, so takes never serve bytes the
    /// stream already forgot.
    #[test]
    fn f06_multi_eviction_drops_same_payloads() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "latest", 4, 68, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.deliver_notification_value(path, &[0x0a; 30])?;
        central.deliver_notification_value(path, &[0x0b; 30])?;
        // 40 bytes fit only after evicting BOTH 30-byte payloads.
        let outcome = central.deliver_notification_value(path, &[0x0c; 40])?;
        check(
            outcome[0].1 == DeliveryOutcome::OverflowNoticed,
            "multi-evict replaces with notice",
        );
        let (items, bytes, slots) = f06_ledger(&central, path, "app-a")?;
        check(items == 1 && bytes == 40 && slots == 1, "only C retained");
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x0c; 40]),
            "take serves C, never evicted A/B",
        );
        check(
            central.take_notification_value(path, "app-a").is_none(),
            "nothing else retained",
        );
        Ok(())
    }

    #[test]
    fn m2_length_only_delivery_buffers_nothing() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "error", 4, 256, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        let outcomes = central.deliver_notification(path, 2)?;
        check(
            outcomes[0].1 == DeliveryOutcome::Delivered,
            "accounting still delivers",
        );
        check(
            central.pending_value_count(path, "app-a") == Some(0),
            "no bytes buffered without values",
        );
        check(
            central.take_notification_value(path, "app-a").is_none(),
            "nothing to take",
        );
        Ok(())
    }

    #[test]
    fn m2_overflow_keeps_buffered_values_then_terminal() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "error", 1, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.deliver_notification_value(path, &[0x0a])?;
        // Rejected value raises the terminal without displacing the buffer.
        central.deliver_notification_value(path, &[0x0b])?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Failed),
            "overflow parks the consumer in failed",
        );
        check(
            central.take_notification_value(path, "app-a") == Some(vec![0x0a]),
            "admitted bytes stay observable after the terminal",
        );
        check(
            central.pending_value_count(path, "app-a") == Some(0),
            "drained exactly",
        );
        check(
            central.take_terminal(path, "app-a").is_some(),
            "overflow terminal observed",
        );
        Ok(())
    }

    #[test]
    fn m5_terminal_removal_releases_orphan_cccd() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let _sub = central.subscribe(path, "error", 1, 128, "app-a", 5000, 2000, &mut out)?;
        central.settle_subscribe_enable(path, true, 2001, &mut out)?;
        central.deliver_notification(path, 1)?;
        central.deliver_notification(path, 1)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Failed),
            "overflow-terminal consumer",
        );
        check(central.physical_cccd_enabled(path), "cccd still live");
        // Removing the terminal consumer issues the orphan disable instead
        // of leaking the live CCCD.
        let disabled = central.unsubscribe(path, "app-a", 2002, &mut out)?;
        check(disabled, "orphan disable issued");
        central.settle_subscribe_disable(path, 2003, &mut out)?;
        check(!central.physical_cccd_enabled(path), "cccd released");
        // The untaken terminal survived the disable: the host still owns
        // that observation.
        check(
            central.take_terminal(path, "app-a").is_some(),
            "terminal preserved across orphan disable",
        );
        // The next removal prunes the observed record and stays quiet.
        let again = central.unsubscribe(path, "app-a", 2004, &mut out)?;
        check(!again, "no second disable");
        check(
            central.consumer_state(path, "app-a").is_none(),
            "record pruned, list bounded",
        );
        Ok(())
    }

    #[test]
    fn m5_subscribe_cycles_do_not_grow_the_consumer_list() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        for round in 0..3u64 {
            let _sub =
                central.subscribe(path, "error", 1, 128, "app-a", 5000, 2000 + round, &mut out)?;
            central.settle_subscribe_enable(path, true, 2010 + round, &mut out)?;
            central.deliver_notification(path, 1)?;
            central.deliver_notification(path, 1)?;
            check(
                central.take_terminal(path, "app-a").is_some(),
                "terminal observed each cycle",
            );
            let disabled = central.unsubscribe(path, "app-a", 2020 + round, &mut out)?;
            check(disabled, "orphan disable each cycle");
            central.settle_subscribe_disable(path, 2030 + round, &mut out)?;
            let _pruned = central.unsubscribe(path, "app-a", 2040 + round, &mut out)?;
        }
        let _sub = central.subscribe(path, "error", 1, 128, "app-a", 5000, 3000, &mut out)?;
        check(
            central.consumer_state(path, "app-a") == Some(ConsumerState::Enabling),
            "resubscribe works after cycles",
        );
        Ok(())
    }

    #[test]
    fn f15_destroy_progresses_with_small_batches() -> Result<(), CoreError> {
        use crate::contracts::ContenderKind;
        use crate::ownership::EffectBatch;

        let config = CentralConfig::new(16, 16, 128, 32, 64, 256, KernelConfig::default())?;
        let mut central = Central::new(fixture_attachment()?, Generation::new("g1")?, config)?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        // Release the setup connect op so the destroy workload is exactly the
        // 33 subscribes below (plus none leftover).
        for id in central.terminal_operation_ids() {
            central.report_release_success(&id)?;
        }
        let _ = central.drain_typed_effects();
        let _ = out.drain();
        // 33 queued subscribes across distinct owners (consumers), 5 of them
        // dispatched to cover both queued and dispatched destroy paths.
        let mut ops = Vec::new();
        for i in 0..33u64 {
            let consumer = format!("consumer-{i}");
            let op =
                central.subscribe(path, "error", 4, 512, &consumer, 5000, 2000 + i, &mut out)?;
            let _ = out.drain();
            let _ = central.drain_typed_effects();
            if i < 5 {
                central.dispatch_op(&op, &mut out)?;
                let _ = out.drain();
            }
            ops.push(op);
        }
        check(
            central.live_operation_ids().len() == 33,
            "33 live ops before destroy",
        );
        // Incremental destruction with a small 16-effect batch: 8 queued
        // settlements per pass, so several passes are required. The host
        // executes each batch outside the lock (here: drain), settles
        // dispatched work as destroyed, and acks every terminal — one failure
        // among successes so the test never blind-acks.
        let mut destroy_out = EffectBatch::new(16);
        let mut steps = 0usize;
        let mut released = 0usize;
        let mut failed_one = false;
        loop {
            let progress = central.destroy_step(&mut destroy_out)?;
            steps += 1;
            // Execute effects outside the lock: drain the batch (the test
            // boundary has no OS work for these logical ops).
            let _effects = destroy_out.drain();
            // Settle dispatched remainders as destroyed before acking.
            for id in central.live_operation_ids() {
                if central.operation_state(&id) == Some(OpStateView::Dispatched) {
                    let mut settle_out = EffectBatch::new(64);
                    central.settle_op(
                        &id,
                        ContenderKind::Destroy,
                        true,
                        0,
                        5000,
                        &mut settle_out,
                    )?;
                    let _ = settle_out.drain();
                }
            }
            // Ack every terminal: exactly one failure, rest success.
            for id in central.terminal_operation_ids() {
                if !failed_one {
                    central.report_release_failure(&id, BleErrorCode::PlatformFailure)?;
                    failed_one = true;
                } else {
                    central.report_release_success(&id)?;
                }
                released += 1;
            }
            let _ = central.drain_typed_effects();
            if progress.done {
                break;
            }
            check(steps < 20, "finite progress, no infinite truncation");
            check(
                progress.truncated
                    || progress.terminal_pending_release > 0
                    || progress.live_operations > 0,
                "progress reports pending work",
            );
        }
        check(steps > 1, "small batch requires more than one pass");
        check(released == 33, "all 33 releases acked exactly once");
        check(central.live_operation_count() == 0, "no live ops remain");
        let record = central.destroy_record()?;
        check(
            record.state() == CleanupState::ReleaseFailed,
            "one failed release preserves ReleaseFailed",
        );
        check(
            record.failures().len() == 1,
            "exactly one failure preserved",
        );
        // Legacy `destroy` with a small batch also progresses (drained loop),
        // never `central.destroy.truncated`.
        let mut central2 = fixture_central()?;
        let mut out2 = batch();
        let (_peer2, _path2) = live_characteristic(&mut central2, &mut out2)?;
        let mut small = EffectBatch::new(8);
        let _record2 = central2.destroy(&mut small)?;
        Ok(())
    }

    #[test]
    fn f25_invalid_contender_cannot_settle_live_work() -> Result<(), CoreError> {
        let mut central = fixture_central()?;
        let mut out = batch();
        let (_peer, path) = live_characteristic(&mut central, &mut out)?;
        let first = central.start_read(path, 5000, 2000, &mut out)?;
        central.dispatch_op(&first, &mut out)?;
        let second = central.start_read(path, 5000, 2001, &mut out)?;
        central.dispatch_op(&second, &mut out)?;
        let outcome =
            central.settle_op(&first, ContenderKind::Failure, false, 0, 2002, &mut out)?;
        check(
            outcome == CompletionOutcome::ContenderIgnored,
            "invalid contender ignored",
        );
        check(
            matches!(
                central.operation_state(&first),
                Some(OpStateView::Dispatched)
            ),
            "live op stays dispatched after invalid contender",
        );
        check(
            matches!(
                central.operation_state(&second),
                Some(OpStateView::Dispatched)
            ),
            "unrelated live work untouched",
        );
        let outcome = central.settle_op(&first, ContenderKind::Failure, true, 1, 2003, &mut out)?;
        check(
            matches!(
                outcome,
                CompletionOutcome::Settled {
                    kind: OperationTerminalKind::Failed,
                    ..
                }
            ),
            "genuine valid failure settles",
        );
        central.report_release_success(&first)?;
        check(
            matches!(
                central.operation_state(&second),
                Some(OpStateView::Dispatched)
            ),
            "second op still live after first settles",
        );
        Ok(())
    }
}
