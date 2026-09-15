//! C-UBM DRAFT mirror: identities, generations, errors, outcomes, bounds,
//! GATT paths, generic-peripheral allowlist, version axes, streams.
//!
//! Derived from `contracts/src/{identities,outcomes,bounds,effects,version,peripheral,streams}.ts`
//! at [`CONTRACT_REVISION`] (pending U1 acceptance). Names are translated from
//! `dotted.code`/`camelCase` to Rust conventions; [`BleErrorCode::as_str`]
//! preserves every frozen wire string verbatim, and `from_str` round-trips it.
//!
//! Derivation notes and intentional divergences:
//! - Time is `u64` monotonic milliseconds ([`MonotonicTime`]). C-UBM uses JS
//!   numbers; sub-millisecond precision is truncated by hosts at the boundary.
//! - `to_deadline` overflow fails closed in both: C-UBM via non-finite `f64`,
//!   here via `checked_add`.
//! - D1 (fixed in C-UBM 0.1.1): `CompletionTerminal::Failed` and
//!   `ContenderKind::Failure` serve the operation machine's
//!   `publish-failure -> failed` edge.
//! - D3 (fixed in C-UBM 0.1.1): stream limits compare like-with-like
//!   (items against reserved item counts, bytes against reserved byte
//!   budgets); `StreamLimits` lives in the `streams` module.
//! - Recovery keeps dispositions verbatim with action kinds only (per AC-04).
//! - Rust tables are `const` and owned values move by value: deep-freeze
//!   (R2) and copy-and-freeze limits (R3) hold by construction.
//! - Canonical decimal form (`^-?[0-9]+$`, no plus/whitespace, 20-digit cap)
//!   is enforced identically in `parse_u64_decimal`/`parse_i64_decimal`.

/// Frozen contract revision this crate mirrors.
pub const CONTRACT_REVISION: &str = "C-UBM.0.1.1-DRAFT";
/// Contract acceptance status.
pub const CONTRACT_STATUS: &str = "DRAFT";
/// Acceptance gate that freezes this draft.
pub const CONTRACT_ACCEPTANCE_GATE: &str = "U1";

/// Millisecond reading of a host-supplied monotonic clock.
pub type MonotonicTime = u64;

/// Frozen BLE error identities, verbatim from C-UBM `BleErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BleErrorCode {
    ProtocolIncompatible,
    ProtocolMalformed,
    ProtocolViolation,
    LifecycleDestroyed,
    LifecycleInvalidState,
    LifecycleInvariantViolation,
    BackendReset,
    AdapterUnavailable,
    AdapterPoweredOff,
    AdapterResetting,
    AdapterSelectionRequired,
    AdapterAmbiguous,
    PermissionDenied,
    PermissionRestricted,
    PermissionNotDetermined,
    OwnershipDenied,
    ConnectionAlreadyOwned,
    ScanAlreadyActive,
    ChooserBusy,
    ArgumentInvalid,
    BytesInvalid,
    BytesTooLarge,
    ScanStartFailed,
    ScanStopFailed,
    ScanFilterInvalid,
    ChooserCancelled,
    ChooserClosed,
    ChooserUserActivationRequired,
    ChooserInsecureContext,
    ChooserApiUnavailable,
    ChooserOptionalServiceNotGranted,
    ChooserPermittedDeviceUnavailable,
    ConnectionNotFound,
    ConnectionFailed,
    ConnectionStale,
    ConnectionLost,
    PeerReferenceInvalid,
    PeerReferenceVersionUnsupported,
    PeerScopeMismatch,
    PeerNotFound,
    OperationAborted,
    OperationTimedOut,
    OperationDisconnected,
    OperationCancelledByDestroy,
    OperationReset,
    OperationAdapterUnavailable,
    GattDiscoveryRequired,
    GattAmbiguousPath,
    GattStaleHandle,
    GattCacheUnknown,
    GattNotFound,
    GattPropertyNotSupported,
    GattReadFailed,
    GattWriteFailed,
    GattSubscribeFailed,
    GattCccdManaged,
    StreamOverflow,
    StreamClosed,
    StreamQuota,
    StreamRateLimited,
    CapabilityUnsupported,
    CapabilityUnavailable,
    CapabilityLimited,
    BackgroundTerminated,
    PlatformFailure,
    PlatformSecurity,
    PlatformTransport,
}

impl BleErrorCode {
    /// Frozen wire string for this code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProtocolIncompatible => "protocol.incompatible",
            Self::ProtocolMalformed => "protocol.malformed",
            Self::ProtocolViolation => "protocol.violation",
            Self::LifecycleDestroyed => "lifecycle.destroyed",
            Self::LifecycleInvalidState => "lifecycle.invalid-state",
            Self::LifecycleInvariantViolation => "lifecycle.invariant-violation",
            Self::BackendReset => "backend.reset",
            Self::AdapterUnavailable => "adapter.unavailable",
            Self::AdapterPoweredOff => "adapter.powered-off",
            Self::AdapterResetting => "adapter.resetting",
            Self::AdapterSelectionRequired => "adapter.selection-required",
            Self::AdapterAmbiguous => "adapter.ambiguous",
            Self::PermissionDenied => "permission.denied",
            Self::PermissionRestricted => "permission.restricted",
            Self::PermissionNotDetermined => "permission.not-determined",
            Self::OwnershipDenied => "ownership.denied",
            Self::ConnectionAlreadyOwned => "connection.already-owned",
            Self::ScanAlreadyActive => "scan.already-active",
            Self::ChooserBusy => "chooser.busy",
            Self::ArgumentInvalid => "argument.invalid",
            Self::BytesInvalid => "bytes.invalid",
            Self::BytesTooLarge => "bytes.too-large",
            Self::ScanStartFailed => "scan.start-failed",
            Self::ScanStopFailed => "scan.stop-failed",
            Self::ScanFilterInvalid => "scan.filter-invalid",
            Self::ChooserCancelled => "chooser.cancelled",
            Self::ChooserClosed => "chooser.closed",
            Self::ChooserUserActivationRequired => "chooser.user-activation-required",
            Self::ChooserInsecureContext => "chooser.insecure-context",
            Self::ChooserApiUnavailable => "chooser.api-unavailable",
            Self::ChooserOptionalServiceNotGranted => "chooser.optional-service-not-granted",
            Self::ChooserPermittedDeviceUnavailable => "chooser.permitted-device-unavailable",
            Self::ConnectionNotFound => "connection.not-found",
            Self::ConnectionFailed => "connection.failed",
            Self::ConnectionStale => "connection.stale",
            Self::ConnectionLost => "connection.lost",
            Self::PeerReferenceInvalid => "peer.reference-invalid",
            Self::PeerReferenceVersionUnsupported => "peer.reference-version-unsupported",
            Self::PeerScopeMismatch => "peer.scope-mismatch",
            Self::PeerNotFound => "peer.not-found",
            Self::OperationAborted => "operation.aborted",
            Self::OperationTimedOut => "operation.timed-out",
            Self::OperationDisconnected => "operation.disconnected",
            Self::OperationCancelledByDestroy => "operation.cancelled-by-destroy",
            Self::OperationReset => "operation.reset",
            Self::OperationAdapterUnavailable => "operation.adapter-unavailable",
            Self::GattDiscoveryRequired => "gatt.discovery-required",
            Self::GattAmbiguousPath => "gatt.ambiguous-path",
            Self::GattStaleHandle => "gatt.stale-handle",
            Self::GattCacheUnknown => "gatt.cache-unknown",
            Self::GattNotFound => "gatt.not-found",
            Self::GattPropertyNotSupported => "gatt.property-not-supported",
            Self::GattReadFailed => "gatt.read-failed",
            Self::GattWriteFailed => "gatt.write-failed",
            Self::GattSubscribeFailed => "gatt.subscribe-failed",
            Self::GattCccdManaged => "gatt.cccd-managed",
            Self::StreamOverflow => "stream.overflow",
            Self::StreamClosed => "stream.closed",
            Self::StreamQuota => "stream.quota",
            Self::StreamRateLimited => "stream.rate-limited",
            Self::CapabilityUnsupported => "capability.unsupported",
            Self::CapabilityUnavailable => "capability.unavailable",
            Self::CapabilityLimited => "capability.limited",
            Self::BackgroundTerminated => "background.terminated",
            Self::PlatformFailure => "platform.failure",
            Self::PlatformSecurity => "platform.security",
            Self::PlatformTransport => "platform.transport",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown codes.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        // `const` string equality keeps this usable in const contexts; the
        // table order follows the C-UBM catalog.
        macro_rules! table {
            ($(($text:literal, $variant:ident)),* $(,)?) => {{
                $(if string_eq(value, $text) {
                    return Some(Self::$variant);
                })*
                None
            }};
        }
        table!(
            ("protocol.incompatible", ProtocolIncompatible),
            ("protocol.malformed", ProtocolMalformed),
            ("protocol.violation", ProtocolViolation),
            ("lifecycle.destroyed", LifecycleDestroyed),
            ("lifecycle.invalid-state", LifecycleInvalidState),
            ("lifecycle.invariant-violation", LifecycleInvariantViolation),
            ("backend.reset", BackendReset),
            ("adapter.unavailable", AdapterUnavailable),
            ("adapter.powered-off", AdapterPoweredOff),
            ("adapter.resetting", AdapterResetting),
            ("adapter.selection-required", AdapterSelectionRequired),
            ("adapter.ambiguous", AdapterAmbiguous),
            ("permission.denied", PermissionDenied),
            ("permission.restricted", PermissionRestricted),
            ("permission.not-determined", PermissionNotDetermined),
            ("ownership.denied", OwnershipDenied),
            ("connection.already-owned", ConnectionAlreadyOwned),
            ("scan.already-active", ScanAlreadyActive),
            ("chooser.busy", ChooserBusy),
            ("argument.invalid", ArgumentInvalid),
            ("bytes.invalid", BytesInvalid),
            ("bytes.too-large", BytesTooLarge),
            ("scan.start-failed", ScanStartFailed),
            ("scan.stop-failed", ScanStopFailed),
            ("scan.filter-invalid", ScanFilterInvalid),
            ("chooser.cancelled", ChooserCancelled),
            ("chooser.closed", ChooserClosed),
            (
                "chooser.user-activation-required",
                ChooserUserActivationRequired
            ),
            ("chooser.insecure-context", ChooserInsecureContext),
            ("chooser.api-unavailable", ChooserApiUnavailable),
            (
                "chooser.optional-service-not-granted",
                ChooserOptionalServiceNotGranted
            ),
            (
                "chooser.permitted-device-unavailable",
                ChooserPermittedDeviceUnavailable
            ),
            ("connection.not-found", ConnectionNotFound),
            ("connection.failed", ConnectionFailed),
            ("connection.stale", ConnectionStale),
            ("connection.lost", ConnectionLost),
            ("peer.reference-invalid", PeerReferenceInvalid),
            (
                "peer.reference-version-unsupported",
                PeerReferenceVersionUnsupported
            ),
            ("peer.scope-mismatch", PeerScopeMismatch),
            ("peer.not-found", PeerNotFound),
            ("operation.aborted", OperationAborted),
            ("operation.timed-out", OperationTimedOut),
            ("operation.disconnected", OperationDisconnected),
            (
                "operation.cancelled-by-destroy",
                OperationCancelledByDestroy
            ),
            ("operation.reset", OperationReset),
            ("operation.adapter-unavailable", OperationAdapterUnavailable),
            ("gatt.discovery-required", GattDiscoveryRequired),
            ("gatt.ambiguous-path", GattAmbiguousPath),
            ("gatt.stale-handle", GattStaleHandle),
            ("gatt.cache-unknown", GattCacheUnknown),
            ("gatt.not-found", GattNotFound),
            ("gatt.property-not-supported", GattPropertyNotSupported),
            ("gatt.read-failed", GattReadFailed),
            ("gatt.write-failed", GattWriteFailed),
            ("gatt.subscribe-failed", GattSubscribeFailed),
            ("gatt.cccd-managed", GattCccdManaged),
            ("stream.overflow", StreamOverflow),
            ("stream.closed", StreamClosed),
            ("stream.quota", StreamQuota),
            ("stream.rate-limited", StreamRateLimited),
            ("capability.unsupported", CapabilityUnsupported),
            ("capability.unavailable", CapabilityUnavailable),
            ("capability.limited", CapabilityLimited),
            ("background.terminated", BackgroundTerminated),
            ("platform.failure", PlatformFailure),
            ("platform.security", PlatformSecurity),
            ("platform.transport", PlatformTransport),
        )
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

/// Frozen error domains, verbatim from C-UBM `BleErrorDomain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BleErrorDomain {
    Core,
    Adapter,
    Scan,
    Chooser,
    Connection,
    Gatt,
    Stream,
    Capability,
    Boundary,
    Cleanup,
    Restoration,
    Ipc,
    Platform,
}

impl BleErrorDomain {
    /// Frozen wire string for this domain.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Adapter => "adapter",
            Self::Scan => "scan",
            Self::Chooser => "chooser",
            Self::Connection => "connection",
            Self::Gatt => "gatt",
            Self::Stream => "stream",
            Self::Capability => "capability",
            Self::Boundary => "boundary",
            Self::Cleanup => "cleanup",
            Self::Restoration => "restoration",
            Self::Ipc => "ipc",
            Self::Platform => "platform",
        }
    }
}

/// Typed kernel failure: frozen code plus domain plus the operation that
/// raised it. Mirrors C-UBM `ContractError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreError {
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: String,
}

impl CoreError {
    /// Build an error. An empty operation is itself `argument.invalid`: the
    /// kernel never emits an error it cannot attribute.
    #[must_use]
    pub fn new(code: BleErrorCode, domain: BleErrorDomain, operation: impl Into<String>) -> Self {
        let operation = operation.into();
        if operation.is_empty() {
            return Self {
                code: BleErrorCode::ArgumentInvalid,
                domain: BleErrorDomain::Core,
                operation: String::from("contract-error.operation"),
            };
        }
        Self {
            code,
            domain,
            operation,
        }
    }

    /// Frozen error code.
    #[must_use]
    pub const fn code(&self) -> BleErrorCode {
        self.code
    }

    /// Error domain.
    #[must_use]
    pub const fn domain(&self) -> BleErrorDomain {
        self.domain
    }

    /// Operation path that raised the error.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }
}

impl core::fmt::Display for CoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} [{}] {}",
            self.code.as_str(),
            self.domain.as_str(),
            self.operation
        )
    }
}

impl std::error::Error for CoreError {}

/// Terminal outcome of one operation, verbatim from C-UBM
/// `OperationTerminalKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationTerminalKind {
    Succeeded,
    Failed,
    Aborted,
    TimedOut,
    Disconnected,
    Reset,
    AdapterUnavailable,
    Destroyed,
}

impl OperationTerminalKind {
    /// Frozen wire string for this terminal kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::TimedOut => "timed-out",
            Self::Disconnected => "disconnected",
            Self::Reset => "reset",
            Self::AdapterUnavailable => "adapter-unavailable",
            Self::Destroyed => "destroyed",
        }
    }
}

/// Exactly one terminal outcome per operation. A succeeded operation carries
/// no cause; every other terminal carries exactly one cause code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRecord {
    operation_id: OperationId,
    kind: OperationTerminalKind,
    cause: Option<BleErrorCode>,
    ingress_ordinal: u64,
    started_at: MonotonicTime,
    settled_at: MonotonicTime,
}

impl TerminalRecord {
    /// Validate and build a terminal record.
    pub fn new(
        operation_id: OperationId,
        kind: OperationTerminalKind,
        cause: Option<BleErrorCode>,
        ingress_ordinal: u64,
        started_at: MonotonicTime,
        settled_at: MonotonicTime,
    ) -> Result<Self, CoreError> {
        let cause_ok = match kind {
            OperationTerminalKind::Succeeded => cause.is_none(),
            _ => cause.is_some(),
        };
        if !cause_ok {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "terminal-record.cause",
            ));
        }
        if settled_at < started_at {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "terminal-record.timing",
            ));
        }
        Ok(Self {
            operation_id,
            kind,
            cause,
            ingress_ordinal,
            started_at,
            settled_at,
        })
    }

    /// Admitted operation identity.
    #[must_use]
    pub const fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Terminal kind.
    #[must_use]
    pub const fn kind(&self) -> OperationTerminalKind {
        self.kind
    }

    /// Cause code (`None` only for `Succeeded`).
    #[must_use]
    pub const fn cause(&self) -> Option<BleErrorCode> {
        self.cause
    }

    /// Serialization-authority ordinal of the winning contender.
    #[must_use]
    pub const fn ingress_ordinal(&self) -> u64 {
        self.ingress_ordinal
    }

    /// Admission instant.
    #[must_use]
    pub const fn started_at(&self) -> MonotonicTime {
        self.started_at
    }

    /// Settlement instant.
    #[must_use]
    pub const fn settled_at(&self) -> MonotonicTime {
        self.settled_at
    }
}

/// One externally visible race contender, verbatim from C-UBM `ContenderKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContenderKind {
    Success,
    Failure,
    Abort,
    Timeout,
    Disconnect,
    Reset,
    Destroy,
    AdapterLoss,
    SessionStop,
    DispatchBegin,
}

impl ContenderKind {
    /// Frozen wire string for this contender kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Abort => "abort",
            Self::Timeout => "timeout",
            Self::Disconnect => "disconnect",
            Self::Reset => "reset",
            Self::Destroy => "destroy",
            Self::AdapterLoss => "adapter-loss",
            Self::SessionStop => "session-stop",
            Self::DispatchBegin => "dispatch-begin",
        }
    }
}

/// A contender offered for arbitration of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contender {
    /// Serialization-authority ordinal assigned by the host.
    pub ingress_ordinal: u64,
    /// What happened.
    pub kind: ContenderKind,
    /// Invalid, duplicate, or stale callbacks never contend.
    pub valid: bool,
}

/// Radio commit disposition after arbitration (OPS-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommitState {
    Committed,
    NotDispatched,
    Unknown,
    Released,
}

impl CommitState {
    /// Frozen wire string for this commit state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::NotDispatched => "not-dispatched",
            Self::Unknown => "unknown",
            Self::Released => "released",
        }
    }
}

/// Completion terminal, verbatim from C-UBM `CompletionTerminal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionTerminal {
    Succeeded,
    Failed,
    Aborted,
    TimedOut,
    Disconnected,
    Reset,
    AdapterUnavailable,
    Destroyed,
}

impl CompletionTerminal {
    /// Frozen wire string for this completion terminal.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::TimedOut => "timed-out",
            Self::Disconnected => "disconnected",
            Self::Reset => "reset",
            Self::AdapterUnavailable => "adapter-unavailable",
            Self::Destroyed => "destroyed",
        }
    }
}

/// Map a winning contender kind to its terminal, mirroring C-UBM
/// `terminalForWinner` exactly.
#[must_use]
pub const fn terminal_for_winner(kind: ContenderKind) -> CompletionTerminal {
    match kind {
        ContenderKind::Success | ContenderKind::DispatchBegin => CompletionTerminal::Succeeded,
        ContenderKind::Failure => CompletionTerminal::Failed,
        ContenderKind::Abort | ContenderKind::SessionStop => CompletionTerminal::Aborted,
        ContenderKind::Timeout => CompletionTerminal::TimedOut,
        ContenderKind::Disconnect => CompletionTerminal::Disconnected,
        ContenderKind::Reset => CompletionTerminal::Reset,
        ContenderKind::AdapterLoss => CompletionTerminal::AdapterUnavailable,
        ContenderKind::Destroy => CompletionTerminal::Destroyed,
    }
}

/// Whether the winner reached the radio: dispatched work that was not
/// pre-dispatched-aborted.
#[must_use]
pub const fn reached_radio_for(dispatched: bool, kind: ContenderKind) -> bool {
    dispatched && !matches!(kind, ContenderKind::Abort)
}

/// Whether paths are invalid before settlement for this winner.
#[must_use]
pub const fn paths_invalid_for(kind: ContenderKind) -> bool {
    matches!(
        kind,
        ContenderKind::Disconnect | ContenderKind::Reset | ContenderKind::AdapterLoss
    )
}

/// Commit disposition for a winner, mirroring C-UBM arbitration exactly.
#[must_use]
pub const fn commit_for(dispatched: bool, kind: ContenderKind) -> CommitState {
    match kind {
        ContenderKind::Success => CommitState::Committed,
        ContenderKind::Abort if !dispatched => CommitState::NotDispatched,
        ContenderKind::Timeout if dispatched => CommitState::Unknown,
        ContenderKind::DispatchBegin => CommitState::NotDispatched,
        _ => CommitState::Released,
    }
}

/// Recovery disposition, verbatim from C-UBM `RecoveryDisposition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryDisposition {
    None,
    RetryImmediately,
    RetryWithBackoff,
    AfterStateChange,
    AfterUserAction,
    CallerPolicy,
}

/// Recovery action kind, verbatim from C-UBM `RecoveryActionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryActionKind {
    RequestPermission,
    OpenSettings,
    WaitForAdapter,
    Rescan,
    ReselectPeer,
    Reconnect,
    RediscoverGatt,
    SelectGattOccurrence,
    Pair,
    Repair,
    ReducePayload,
    WaitForWriteReady,
    RecreateManager,
    Retry,
}

/// Recovery decision for one error code: disposition plus action kinds only
/// (AC-04). Mirrors C-UBM `recoveryFor` exactly.
#[must_use]
pub const fn recovery_for(
    code: BleErrorCode,
) -> (RecoveryDisposition, &'static [RecoveryActionKind]) {
    use BleErrorCode as C;
    use RecoveryActionKind as A;
    use RecoveryDisposition as D;
    match code {
        C::ProtocolIncompatible
        | C::ProtocolMalformed
        | C::ProtocolViolation
        | C::LifecycleInvariantViolation
        | C::ArgumentInvalid
        | C::BytesInvalid
        | C::OwnershipDenied
        | C::ConnectionAlreadyOwned
        | C::ScanAlreadyActive
        | C::ChooserBusy
        | C::PeerReferenceInvalid
        | C::PeerReferenceVersionUnsupported
        | C::PeerScopeMismatch
        | C::GattNotFound
        | C::GattPropertyNotSupported
        | C::GattReadFailed
        | C::GattWriteFailed
        | C::GattSubscribeFailed
        | C::CapabilityUnsupported
        | C::CapabilityUnavailable
        | C::CapabilityLimited => (D::None, &[]),
        C::BytesTooLarge => (D::None, &[A::ReducePayload]),
        C::LifecycleDestroyed
        | C::LifecycleInvalidState
        | C::BackendReset
        | C::OperationCancelledByDestroy => (D::None, &[A::RecreateManager]),
        C::AdapterUnavailable
        | C::AdapterResetting
        | C::AdapterAmbiguous
        | C::OperationAdapterUnavailable => (D::AfterStateChange, &[A::WaitForAdapter]),
        C::AdapterPoweredOff => (D::AfterStateChange, &[A::WaitForAdapter]),
        C::AdapterSelectionRequired => (D::AfterUserAction, &[A::ReselectPeer]),
        C::PermissionDenied => (D::AfterUserAction, &[A::RequestPermission, A::OpenSettings]),
        C::PermissionRestricted => (D::AfterUserAction, &[A::OpenSettings]),
        C::PermissionNotDetermined => (D::AfterUserAction, &[A::RequestPermission]),
        C::ScanStartFailed | C::ScanStopFailed | C::ScanFilterInvalid => (D::None, &[A::Rescan]),
        C::ChooserCancelled
        | C::ChooserClosed
        | C::ChooserUserActivationRequired
        | C::ChooserInsecureContext
        | C::ChooserApiUnavailable
        | C::ChooserOptionalServiceNotGranted
        | C::ChooserPermittedDeviceUnavailable => (D::AfterUserAction, &[A::ReselectPeer]),
        C::ConnectionNotFound
        | C::ConnectionFailed
        | C::ConnectionStale
        | C::ConnectionLost
        | C::OperationDisconnected
        | C::OperationReset => (D::RetryWithBackoff, &[A::Reconnect]),
        C::PeerNotFound => (D::CallerPolicy, &[A::Rescan]),
        C::GattDiscoveryRequired | C::GattStaleHandle | C::GattCacheUnknown => {
            (D::RetryImmediately, &[A::RediscoverGatt])
        }
        C::GattAmbiguousPath => (D::CallerPolicy, &[A::SelectGattOccurrence]),
        C::GattCccdManaged => (D::None, &[A::WaitForWriteReady]),
        C::StreamOverflow | C::StreamClosed | C::StreamQuota | C::StreamRateLimited => {
            (D::RetryWithBackoff, &[A::Retry])
        }
        C::BackgroundTerminated => (D::AfterStateChange, &[A::Reconnect]),
        C::PlatformFailure | C::PlatformTransport => (D::CallerPolicy, &[]),
        C::PlatformSecurity => (D::AfterUserAction, &[A::Pair, A::Repair]),
        C::OperationAborted | C::OperationTimedOut => (D::CallerPolicy, &[A::Retry]),
    }
}

/// Define one validated non-empty string newtype (AC-02: structural records
/// with validated fields, never bare asserted strings).
macro_rules! string_id {
    ($name:ident, $path:literal) => {
        #[doc = concat!("Validated `", $path, "` identifier.")]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validate and wrap a `", $path, "` value.")]
            pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(CoreError::new(
                        BleErrorCode::ArgumentInvalid,
                        BleErrorDomain::Core,
                        $path,
                    ));
                }
                Ok(Self(value))
            }

            /// Borrow the validated value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(AttachmentId, "attachment.attachment-id");
string_id!(BackendInstanceId, "attachment.backend-instance-id");
string_id!(BackendGeneration, "attachment.backend-generation");
string_id!(AdapterId, "attachment.adapter-id");
string_id!(AdapterGeneration, "attachment.adapter-generation");
string_id!(OperationId, "kernel.operation-id");
string_id!(LeaseId, "kernel.owner-lease");
string_id!(ConnectionGeneration, "gatt-path.connection-generation");
string_id!(DatabaseGeneration, "gatt-path.database-generation");
string_id!(Generation, "kernel.generation");

/// The unrepeatable tuple that scopes all handles and correlations. A handle
/// from backend instance A is stale at instance B even when visible ids
/// repeat. Mirrors C-UBM `AttachmentTuple`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentTuple {
    attachment_id: AttachmentId,
    backend_instance_id: BackendInstanceId,
    backend_generation: BackendGeneration,
    adapter_id: AdapterId,
    adapter_generation: AdapterGeneration,
}

impl AttachmentTuple {
    /// Validate and build the tuple. Every field is required and non-empty.
    pub fn new(
        attachment_id: AttachmentId,
        backend_instance_id: BackendInstanceId,
        backend_generation: BackendGeneration,
        adapter_id: AdapterId,
        adapter_generation: AdapterGeneration,
    ) -> Self {
        Self {
            attachment_id,
            backend_instance_id,
            backend_generation,
            adapter_id,
            adapter_generation,
        }
    }

    /// Structural equality: every field must match (OWN-02).
    #[must_use]
    pub fn equals(&self, other: &Self) -> bool {
        self == other
    }

    /// Borrow the attachment id.
    #[must_use]
    pub const fn attachment_id(&self) -> &AttachmentId {
        &self.attachment_id
    }

    /// Borrow the backend instance id.
    #[must_use]
    pub const fn backend_instance_id(&self) -> &BackendInstanceId {
        &self.backend_instance_id
    }

    /// Borrow the backend generation.
    #[must_use]
    pub const fn backend_generation(&self) -> &BackendGeneration {
        &self.backend_generation
    }

    /// Borrow the adapter id.
    #[must_use]
    pub const fn adapter_id(&self) -> &AdapterId {
        &self.adapter_id
    }

    /// Borrow the adapter generation.
    #[must_use]
    pub const fn adapter_generation(&self) -> &AdapterGeneration {
        &self.adapter_generation
    }
}

/// Peer identity domain. Address-like values are merely one possible domain
/// and must not be treated as globally stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerIdentityDomain {
    PublicAddress,
    StaticRandomAddress,
    ResolvablePrivateAddress,
    PlatformGuid,
    OpaqueToken,
}

impl PeerIdentityDomain {
    /// Frozen wire string for this domain.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PublicAddress => "public-address",
            Self::StaticRandomAddress => "static-random-address",
            Self::ResolvablePrivateAddress => "resolvable-private-address",
            Self::PlatformGuid => "platform-guid",
            Self::OpaqueToken => "opaque-token",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown domains.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "public-address") {
            Some(Self::PublicAddress)
        } else if string_eq(value, "static-random-address") {
            Some(Self::StaticRandomAddress)
        } else if string_eq(value, "resolvable-private-address") {
            Some(Self::ResolvablePrivateAddress)
        } else if string_eq(value, "platform-guid") {
            Some(Self::PlatformGuid)
        } else if string_eq(value, "opaque-token") {
            Some(Self::OpaqueToken)
        } else {
            None
        }
    }

    /// Only public and static-random addresses are globally stable.
    #[must_use]
    pub const fn is_globally_stable(self) -> bool {
        matches!(self, Self::PublicAddress | Self::StaticRandomAddress)
    }
}

/// Scoped peer identity: attachment plus domain plus value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    attachment: AttachmentTuple,
    domain: PeerIdentityDomain,
    value: String,
}

impl PeerIdentity {
    /// Validate and build a peer identity.
    pub fn new(
        attachment: AttachmentTuple,
        domain: PeerIdentityDomain,
        value: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "peer-identity.value",
            ));
        }
        Ok(Self {
            attachment,
            domain,
            value,
        })
    }

    /// Session-scoped key for first/merged/latest delivery.
    #[must_use]
    pub fn session_key(&self) -> String {
        let mut key = String::with_capacity(self.domain.as_str().len() + 1 + self.value.len());
        key.push_str(self.domain.as_str());
        key.push(':');
        key.push_str(&self.value);
        key
    }

    /// Borrow the attachment scope.
    #[must_use]
    pub const fn attachment(&self) -> &AttachmentTuple {
        &self.attachment
    }

    /// Identity domain.
    #[must_use]
    pub const fn domain(&self) -> PeerIdentityDomain {
        self.domain
    }

    /// Identity value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Generations are opaque and unrepeatable: only the current generation is
/// usable, and a stale object is never revived.
#[must_use]
pub fn is_generation_current(used: &Generation, current: &Generation) -> bool {
    used == current
}

/// Reject a foreign or stale handle before any radio effect (OWN-02).
pub fn assert_same_attachment(
    handle_attachment: &AttachmentTuple,
    current_attachment: &AttachmentTuple,
    operation: &str,
) -> Result<(), CoreError> {
    if !handle_attachment.equals(current_attachment) {
        return Err(CoreError::new(
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
            String::from(operation),
        ));
    }
    Ok(())
}

/// GATT occurrence path with construction invariants (R9): a characteristic
/// UUID requires its occurrence and vice versa, the same pairing holds for
/// descriptors, a descriptor requires a characteristic, and the path
/// attachment must equal the peer attachment scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GattPath {
    attachment: AttachmentTuple,
    peer: PeerIdentity,
    connection_generation: Generation,
    database_generation: Generation,
    service_uuid: String,
    service_occurrence: u64,
    characteristic_uuid: Option<String>,
    characteristic_occurrence: Option<u64>,
    descriptor_uuid: Option<String>,
    descriptor_occurrence: Option<u64>,
    owner_lease: LeaseId,
}

/// Validated inputs for [`GattPath::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GattPathParams {
    /// Path attachment scope.
    pub attachment: AttachmentTuple,
    /// Peer identity carrying its own attachment scope.
    pub peer: PeerIdentity,
    /// Connection generation.
    pub connection_generation: Generation,
    /// Database generation.
    pub database_generation: Generation,
    /// Service UUID (non-empty; wire canonicalization at the TS boundary).
    pub service_uuid: String,
    /// Service occurrence.
    pub service_occurrence: u64,
    /// Characteristic UUID and occurrence (paired).
    pub characteristic_uuid: Option<String>,
    /// Characteristic UUID and occurrence (paired).
    pub characteristic_occurrence: Option<u64>,
    /// Descriptor UUID and occurrence (paired; requires a characteristic).
    pub descriptor_uuid: Option<String>,
    /// Descriptor UUID and occurrence (paired; requires a characteristic).
    pub descriptor_occurrence: Option<u64>,
    /// Owner lease.
    pub owner_lease: LeaseId,
}

impl GattPath {
    /// Validate and build a path. UUID strings must be non-empty (wire
    /// canonicalization is enforced at the TS boundary); pairing and scope
    /// are enforced here.
    pub fn new(params: GattPathParams) -> Result<Self, CoreError> {
        let GattPathParams {
            attachment,
            peer,
            connection_generation,
            database_generation,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
            owner_lease,
        } = params;
        if service_uuid.is_empty() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.service-uuid",
            ));
        }
        if characteristic_uuid.as_ref().is_some_and(String::is_empty) {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.characteristic-uuid",
            ));
        }
        if descriptor_uuid.as_ref().is_some_and(String::is_empty) {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.descriptor-uuid",
            ));
        }
        if characteristic_uuid.is_some() != characteristic_occurrence.is_some() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.characteristic-pairing",
            ));
        }
        if descriptor_uuid.is_some() != descriptor_occurrence.is_some() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.descriptor-pairing",
            ));
        }
        if descriptor_uuid.is_some() && characteristic_uuid.is_none() {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "gatt-path.descriptor-without-characteristic",
            ));
        }
        if !attachment.equals(peer.attachment()) {
            return Err(CoreError::new(
                BleErrorCode::PeerScopeMismatch,
                BleErrorDomain::Connection,
                "gatt-path.peer-scope",
            ));
        }
        Ok(Self {
            attachment,
            peer,
            connection_generation,
            database_generation,
            service_uuid,
            service_occurrence,
            characteristic_uuid,
            characteristic_occurrence,
            descriptor_uuid,
            descriptor_occurrence,
            owner_lease,
        })
    }

    /// Borrow the path attachment scope.
    #[must_use]
    pub const fn attachment(&self) -> &AttachmentTuple {
        &self.attachment
    }

    /// Borrow the peer identity.
    #[must_use]
    pub const fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    /// Borrow the owner lease.
    #[must_use]
    pub const fn owner_lease(&self) -> &LeaseId {
        &self.owner_lease
    }
}

/// Fail-closed generic-shape allowlist (R8): only the frozen generic keys
/// are admitted; any unlisted key — including physiological or commercial
/// keys — is rejected. Mirrors C-UBM `GENERIC_PERIPHERAL_ALLOWED_KEYS`.
pub const GENERIC_PERIPHERAL_ALLOWED_KEYS: [&str; 10] = [
    "octetPayload",
    "serviceUuids",
    "manufacturerId",
    "manufacturerPayload",
    "serviceDataUuid",
    "serviceDataPayload",
    "localName",
    "txPowerLevel",
    "flags",
    "appearance",
];

/// Reject any declaration key outside the generic allowlist.
pub fn assert_generic_peripheral_decl(keys: &[&str]) -> Result<(), CoreError> {
    for key in keys {
        let mut allowed = false;
        for candidate in GENERIC_PERIPHERAL_ALLOWED_KEYS {
            if string_eq(key, candidate) {
                allowed = true;
                break;
            }
        }
        if !allowed {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "peripheral.generic-decl",
            ));
        }
    }
    Ok(())
}

// Frozen numeric production limits, verbatim from C-UBM `bounds.ts`.
/// Maximum stream item capacity.
pub const MAX_STREAM_ITEM_CAPACITY: u64 = 65_536;
/// Minimum stream item capacity.
pub const MIN_STREAM_ITEM_CAPACITY: u64 = 1;
/// Maximum stream byte capacity (4 MiB).
pub const MAX_STREAM_BYTE_CAPACITY: u64 = 4_194_304;
/// Client aggregate byte budget (4 MiB).
pub const CLIENT_AGGREGATE_BYTES: u64 = 4_194_304;
/// Backend ingress aggregate byte budget (16 MiB).
pub const BACKEND_INGRESS_AGGREGATE_BYTES: u64 = 16_777_216;
/// Adapter-owner aggregate byte budget (64 MiB).
pub const ADAPTER_OWNER_AGGREGATE_BYTES: u64 = 67_108_864;
/// Effective operation payload ceiling (512 KiB, AC-03).
pub const MAX_OPERATION_BYTES: u64 = 524_288;
/// Maximum scan-state entries.
pub const MAX_SCAN_STATE_ENTRIES: u64 = 256;
/// Maximum scan-state bytes (256 KiB).
pub const MAX_SCAN_STATE_BYTES: u64 = 262_144;
/// Maximum trace bytes (512 KiB).
pub const TRACE_MAX_BYTES: u64 = 524_288;
/// Maximum timeout in milliseconds (`i32::MAX`).
pub const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// Minimum timeout in milliseconds.
pub const MIN_TIMEOUT_MS: u64 = 1;
/// Maximum IPC leases per identity.
pub const MAX_IPC_LEASES_PER_IDENTITY: u64 = 2;

/// Validate a stream item capacity (`1..=65536`).
pub fn assert_item_capacity(value: u64, operation: &str) -> Result<u64, CoreError> {
    if !(MIN_STREAM_ITEM_CAPACITY..=MAX_STREAM_ITEM_CAPACITY).contains(&value) {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Stream,
            String::from(operation),
        ));
    }
    Ok(value)
}

/// Validate a stream byte capacity (`1..=4MiB`; above the ceiling is quota).
pub fn assert_byte_capacity(value: u64, operation: &str) -> Result<u64, CoreError> {
    if value < 1 {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Stream,
            String::from(operation),
        ));
    }
    if value > MAX_STREAM_BYTE_CAPACITY {
        return Err(CoreError::new(
            BleErrorCode::StreamQuota,
            BleErrorDomain::Stream,
            String::from(operation),
        ));
    }
    Ok(value)
}

/// Validate a timeout in milliseconds (`1..=i32::MAX`).
pub fn assert_timeout_ms(value: u64, operation: &str) -> Result<u64, CoreError> {
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&value) {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            String::from(operation),
        ));
    }
    Ok(value)
}

/// Convert a timeout duration to an absolute monotonic deadline at admission.
/// Helpers preserve the earlier deadline, never extend it. Overflow fails
/// closed instead of wrapping.
pub fn to_deadline(now: MonotonicTime, timeout_ms: u64) -> Result<MonotonicTime, CoreError> {
    assert_timeout_ms(timeout_ms, "deadline.timeout")?;
    now.checked_add(timeout_ms).ok_or_else(|| {
        CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "deadline.absolute",
        )
    })
}

/// A deadline expires when `now` reaches it.
#[must_use]
pub const fn is_deadline_expired(now: MonotonicTime, deadline: MonotonicTime) -> bool {
    now >= deadline
}

/// Compose deadlines by keeping the earlier one, never extending.
#[must_use]
pub const fn earliest_deadline(first: MonotonicTime, second: MonotonicTime) -> MonotonicTime {
    if first < second { first } else { second }
}

/// The effective maximum is the minimum of the declared maxima, clamped to
/// the frozen operation ceiling. An unavailable or unmeasured maximum is not
/// infinity: callers must pass only measured values and surface `None`
/// earlier as `capability.unavailable`.
pub fn effective_max_bytes(maxima: &[u64]) -> Result<u64, CoreError> {
    let mut effective = MAX_OPERATION_BYTES;
    let mut seen = false;
    for maximum in maxima {
        if *maximum == 0 {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "bytes.maxima",
            ));
        }
        seen = true;
        if *maximum < effective {
            effective = *maximum;
        }
    }
    if !seen {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "bytes.maxima",
        ));
    }
    Ok(effective)
}

/// Enforce a payload length against declared maxima. `None` is an unmeasured
/// maximum and fails as `capability.unavailable`, never as infinity.
pub fn assert_bytes_within_limit(
    length: u64,
    maxima: &[Option<u64>],
    operation: &str,
) -> Result<(), CoreError> {
    // The effective ceiling is the minimum of the measured maxima, clamped
    // to the frozen operation ceiling; the list is scanned without allocation.
    let mut running_min = MAX_OPERATION_BYTES;
    let mut any = false;
    for maximum in maxima {
        let Some(bound) = maximum else {
            return Err(CoreError::new(
                BleErrorCode::CapabilityUnavailable,
                BleErrorDomain::Capability,
                String::from(operation),
            ));
        };
        if *bound == 0 {
            return Err(CoreError::new(
                BleErrorCode::ArgumentInvalid,
                BleErrorDomain::Core,
                "bytes.maxima",
            ));
        }
        any = true;
        if *bound < running_min {
            running_min = *bound;
        }
    }
    if !any {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "bytes.maxima",
        ));
    }
    if length > running_min {
        return Err(CoreError::new(
            BleErrorCode::BytesTooLarge,
            BleErrorDomain::Core,
            String::from(operation),
        ));
    }
    Ok(())
}

/// Maximum `u64` wire value.
pub const U64_MAX: u64 = u64::MAX;
/// Minimum `u64` wire value.
pub const U64_MIN: u64 = u64::MIN;
/// Maximum `i64` wire value.
pub const I64_MAX: i64 = i64::MAX;
/// Minimum `i64` wire value.
pub const I64_MIN: i64 = i64::MIN;
/// Canonical decimal digit cap (excluding an optional leading `-`).
/// Mirrors C-UBM `MAX_DECIMAL_DIGITS`.
pub const MAX_DECIMAL_DIGITS: usize = 20;

/// Parse a decimal-string `u64` wire value (DATA-02). Canonical form is
/// `^[0-9]+$` with no plus sign, no whitespace, and at most
/// `MAX_DECIMAL_DIGITS` digits; anything else is `bytes.invalid`.
/// Out-of-range is `bytes.invalid`, never a wrap.
pub fn parse_u64_decimal(value: &str) -> Result<u64, CoreError> {
    if value.is_empty() {
        return Err(CoreError::new(
            BleErrorCode::BytesInvalid,
            BleErrorDomain::Core,
            "u64.input",
        ));
    }
    if value.len() > MAX_DECIMAL_DIGITS {
        return Err(CoreError::new(
            BleErrorCode::BytesInvalid,
            BleErrorDomain::Core,
            "u64.input",
        ));
    }
    let mut result: u64 = 0;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "u64.input",
            ));
        }
        let digit = u64::from(byte - b'0');
        result = result
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::BytesInvalid,
                    BleErrorDomain::Core,
                    "u64.range",
                )
            })?;
    }
    Ok(result)
}

/// Parse a decimal-string `i64` wire value (DATA-02). Canonical form is
/// `^-?[0-9]+$`: an optional leading `-` is the only accepted sign (no plus,
/// no whitespace), with at most `MAX_DECIMAL_DIGITS` digits; out-of-range is
/// `bytes.invalid`.
pub fn parse_i64_decimal(value: &str) -> Result<i64, CoreError> {
    let stripped = value.strip_prefix('-');
    let digits = match stripped {
        Some(rest) => rest,
        None => value,
    };
    let negative = digits.len() != value.len();
    if digits.is_empty() {
        return Err(CoreError::new(
            BleErrorCode::BytesInvalid,
            BleErrorDomain::Core,
            "i64.input",
        ));
    }
    if digits.len() > MAX_DECIMAL_DIGITS {
        return Err(CoreError::new(
            BleErrorCode::BytesInvalid,
            BleErrorDomain::Core,
            "i64.input",
        ));
    }
    let mut magnitude: u64 = 0;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            return Err(CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "i64.input",
            ));
        }
        let digit = u64::from(byte - b'0');
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            .ok_or_else(|| {
                CoreError::new(
                    BleErrorCode::BytesInvalid,
                    BleErrorDomain::Core,
                    "i64.range",
                )
            })?;
    }
    if negative {
        if magnitude > (i64::MAX as u64).saturating_add(1) {
            return Err(CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "i64.range",
            ));
        }
        if magnitude == (i64::MAX as u64).saturating_add(1) {
            return Ok(i64::MIN);
        }
        let positive = i64::try_from(magnitude).map_err(|_| {
            CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "i64.range",
            )
        })?;
        positive.checked_neg().ok_or_else(|| {
            CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "i64.range",
            )
        })
    } else {
        i64::try_from(magnitude).map_err(|_| {
            CoreError::new(
                BleErrorCode::BytesInvalid,
                BleErrorDomain::Core,
                "i64.range",
            )
        })
    }
}

/// Runtime handshake axis under negotiation. The six-axis list is closed:
/// unknown wire strings are rejected by [`RuntimeAxis::from_str`], mirroring
/// C-UBM `isRuntimeAxis` (R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeAxis {
    BackendContract,
    CapabilitySchema,
    EventSchema,
    TraceFormat,
    NativeProtocol,
    IpcProtocol,
}

impl RuntimeAxis {
    /// Frozen wire string for this axis.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackendContract => "backend-contract",
            Self::CapabilitySchema => "capability-schema",
            Self::EventSchema => "event-schema",
            Self::TraceFormat => "trace-format",
            Self::NativeProtocol => "native-protocol",
            Self::IpcProtocol => "ipc-protocol",
        }
    }

    /// Parse a frozen wire string. Returns `None` for unknown axes.
    #[must_use]
    pub const fn from_str(value: &str) -> Option<Self> {
        if string_eq(value, "backend-contract") {
            Some(Self::BackendContract)
        } else if string_eq(value, "capability-schema") {
            Some(Self::CapabilitySchema)
        } else if string_eq(value, "event-schema") {
            Some(Self::EventSchema)
        } else if string_eq(value, "trace-format") {
            Some(Self::TraceFormat)
        } else if string_eq(value, "native-protocol") {
            Some(Self::NativeProtocol)
        } else if string_eq(value, "ipc-protocol") {
            Some(Self::IpcProtocol)
        } else {
            None
        }
    }
}

/// One offered version span on an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionSpan {
    /// Negotiated axis.
    pub axis: RuntimeAxis,
    /// Inclusive minimum.
    pub minimum: u64,
    /// Inclusive maximum (`>= minimum`).
    pub maximum: u64,
}

impl VersionSpan {
    /// Validate and build a span.
    pub fn new(axis: RuntimeAxis, minimum: u64, maximum: u64) -> Result<Self, CoreError> {
        if minimum > maximum {
            return Err(CoreError::new(
                BleErrorCode::ProtocolMalformed,
                BleErrorDomain::Core,
                "version-span.range",
            ));
        }
        Ok(Self {
            axis,
            minimum,
            maximum,
        })
    }
}

/// Negotiated selection on one axis: the highest common value wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedAxis {
    /// Negotiated axis.
    pub axis: RuntimeAxis,
    /// Selected version.
    pub selected: u64,
    /// Local offer bounds.
    pub local_minimum: u64,
    /// Local offer bounds.
    pub local_maximum: u64,
    /// Remote offer bounds.
    pub remote_minimum: u64,
    /// Remote offer bounds.
    pub remote_maximum: u64,
}

/// Negotiate one axis. Disjoint ranges fail closed with
/// `protocol.incompatible`: there is no implicit downgrade.
pub fn negotiate_version_span(
    local: VersionSpan,
    remote: VersionSpan,
) -> Result<NegotiatedAxis, CoreError> {
    if local.axis != remote.axis {
        return Err(CoreError::new(
            BleErrorCode::ProtocolMalformed,
            BleErrorDomain::Core,
            "version-negotiate.axes",
        ));
    }
    let selected = local.maximum.min(remote.maximum);
    if selected < local.minimum || selected < remote.minimum {
        return Err(CoreError::new(
            BleErrorCode::ProtocolIncompatible,
            BleErrorDomain::Core,
            "version-negotiate.axis",
        ));
    }
    Ok(NegotiatedAxis {
        axis: local.axis,
        selected,
        local_minimum: local.minimum,
        local_maximum: local.maximum,
        remote_minimum: remote.minimum,
        remote_maximum: remote.maximum,
    })
}

/// A negotiated selection binds only to an offer that contains it.
pub fn assert_negotiated_within_offer(
    selected: NegotiatedAxis,
    offer: VersionSpan,
) -> Result<(), CoreError> {
    if selected.axis != offer.axis {
        return Err(CoreError::new(
            BleErrorCode::ProtocolMalformed,
            BleErrorDomain::Core,
            "version-accepted.axes",
        ));
    }
    if selected.selected < offer.minimum || selected.selected > offer.maximum {
        return Err(CoreError::new(
            BleErrorCode::ProtocolIncompatible,
            BleErrorDomain::Core,
            "version-accepted.axis",
        ));
    }
    Ok(())
}

/// Contract revisions are never silently equal: a mismatch fails closed even
/// when every runtime axis overlaps.
pub fn assert_contract_revision_equal(local: &str, remote: &str) -> Result<(), CoreError> {
    if local != CONTRACT_REVISION || remote != CONTRACT_REVISION || local != remote {
        return Err(CoreError::new(
            BleErrorCode::ProtocolIncompatible,
            BleErrorDomain::Core,
            "contract-revision.mismatch",
        ));
    }
    Ok(())
}

/// Handshake state. Initialization fails before sensor operation when the
/// binding and core identities differ (PKG-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeState {
    /// Whether negotiation completed.
    pub complete: bool,
}

/// No effect dispatches on an incomplete handshake.
pub fn assert_handshake_complete(state: HandshakeState, operation: &str) -> Result<(), CoreError> {
    if !state.complete {
        return Err(CoreError::new(
            BleErrorCode::LifecycleInvalidState,
            BleErrorDomain::Core,
            String::from(operation),
        ));
    }
    if operation.is_empty() {
        return Err(CoreError::new(
            BleErrorCode::ArgumentInvalid,
            BleErrorDomain::Core,
            "handshake.operation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ADAPTER_OWNER_AGGREGATE_BYTES, AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple,
        BACKEND_INGRESS_AGGREGATE_BYTES, BackendGeneration, BackendInstanceId, BleErrorCode,
        BleErrorDomain, CLIENT_AGGREGATE_BYTES, CONTRACT_ACCEPTANCE_GATE, CONTRACT_REVISION,
        CONTRACT_STATUS, CommitState, CompletionTerminal, ContenderKind, CoreError, Generation,
        HandshakeState, I64_MAX, I64_MIN, LeaseId, MAX_IPC_LEASES_PER_IDENTITY,
        MAX_OPERATION_BYTES, MAX_SCAN_STATE_BYTES, MAX_SCAN_STATE_ENTRIES,
        MAX_STREAM_BYTE_CAPACITY, MAX_STREAM_ITEM_CAPACITY, MAX_TIMEOUT_MS,
        MIN_STREAM_ITEM_CAPACITY, MIN_TIMEOUT_MS, NegotiatedAxis, OperationId,
        OperationTerminalKind, PeerIdentity, PeerIdentityDomain, RecoveryDisposition, RuntimeAxis,
        TRACE_MAX_BYTES, TerminalRecord, U64_MAX, U64_MIN, VersionSpan, assert_byte_capacity,
        assert_bytes_within_limit, assert_contract_revision_equal, assert_handshake_complete,
        assert_item_capacity, assert_negotiated_within_offer, assert_same_attachment,
        assert_timeout_ms, commit_for, earliest_deadline, effective_max_bytes, is_deadline_expired,
        is_generation_current, negotiate_version_span, parse_i64_decimal, parse_u64_decimal,
        paths_invalid_for, reached_radio_for, recovery_for, terminal_for_winner, to_deadline,
    };
    use crate::check;

    fn attachment_fixture() -> Option<AttachmentTuple> {
        let id = AttachmentId::new("attach-01").ok()?;
        let instance = BackendInstanceId::new("backend-01").ok()?;
        let generation = BackendGeneration::new("bg-3").ok()?;
        let adapter = AdapterId::new("adapter-01").ok()?;
        let adapter_generation = AdapterGeneration::new("ag-2").ok()?;
        Some(AttachmentTuple::new(
            id,
            instance,
            generation,
            adapter,
            adapter_generation,
        ))
    }

    #[test]
    fn frozen_revision_constants() {
        assert_eq!(CONTRACT_REVISION, "C-UBM.0.1.1-DRAFT");
        assert_eq!(CONTRACT_STATUS, "DRAFT");
        assert_eq!(CONTRACT_ACCEPTANCE_GATE, "U1");
    }

    #[test]
    fn frozen_numeric_table() {
        assert_eq!(MAX_STREAM_ITEM_CAPACITY, 65_536);
        assert_eq!(MIN_STREAM_ITEM_CAPACITY, 1);
        assert_eq!(MAX_STREAM_BYTE_CAPACITY, 4_194_304);
        assert_eq!(CLIENT_AGGREGATE_BYTES, 4_194_304);
        assert_eq!(BACKEND_INGRESS_AGGREGATE_BYTES, 16_777_216);
        assert_eq!(ADAPTER_OWNER_AGGREGATE_BYTES, 67_108_864);
        assert_eq!(MAX_OPERATION_BYTES, 524_288);
        assert_eq!(MAX_SCAN_STATE_ENTRIES, 256);
        assert_eq!(MAX_SCAN_STATE_BYTES, 262_144);
        assert_eq!(TRACE_MAX_BYTES, 524_288);
        assert_eq!(MAX_TIMEOUT_MS, 2_147_483_647);
        assert_eq!(MIN_TIMEOUT_MS, 1);
        assert_eq!(MAX_IPC_LEASES_PER_IDENTITY, 2);
        assert_eq!(U64_MAX, u64::MAX);
        assert_eq!(U64_MIN, u64::MIN);
        assert_eq!(I64_MAX, i64::MAX);
        assert_eq!(I64_MIN, i64::MIN);
    }

    #[test]
    fn error_code_wire_strings_round_trip() {
        let cases: &[(BleErrorCode, &str)] = &[
            (BleErrorCode::ProtocolIncompatible, "protocol.incompatible"),
            (BleErrorCode::ProtocolMalformed, "protocol.malformed"),
            (BleErrorCode::LifecycleDestroyed, "lifecycle.destroyed"),
            (BleErrorCode::BackendReset, "backend.reset"),
            (BleErrorCode::AdapterPoweredOff, "adapter.powered-off"),
            (BleErrorCode::PermissionDenied, "permission.denied"),
            (BleErrorCode::OwnershipDenied, "ownership.denied"),
            (
                BleErrorCode::ConnectionAlreadyOwned,
                "connection.already-owned",
            ),
            (BleErrorCode::ScanAlreadyActive, "scan.already-active"),
            (BleErrorCode::ChooserBusy, "chooser.busy"),
            (BleErrorCode::ArgumentInvalid, "argument.invalid"),
            (BleErrorCode::BytesTooLarge, "bytes.too-large"),
            (BleErrorCode::ScanFilterInvalid, "scan.filter-invalid"),
            (
                BleErrorCode::ChooserOptionalServiceNotGranted,
                "chooser.optional-service-not-granted",
            ),
            (
                BleErrorCode::ChooserPermittedDeviceUnavailable,
                "chooser.permitted-device-unavailable",
            ),
            (BleErrorCode::ConnectionStale, "connection.stale"),
            (
                BleErrorCode::PeerReferenceVersionUnsupported,
                "peer.reference-version-unsupported",
            ),
            (
                BleErrorCode::OperationCancelledByDestroy,
                "operation.cancelled-by-destroy",
            ),
            (
                BleErrorCode::OperationAdapterUnavailable,
                "operation.adapter-unavailable",
            ),
            (BleErrorCode::GattStaleHandle, "gatt.stale-handle"),
            (BleErrorCode::GattCccdManaged, "gatt.cccd-managed"),
            (BleErrorCode::StreamQuota, "stream.quota"),
            (BleErrorCode::CapabilityLimited, "capability.limited"),
            (BleErrorCode::BackgroundTerminated, "background.terminated"),
            (BleErrorCode::PlatformTransport, "platform.transport"),
        ];
        for (code, wire) in cases {
            assert_eq!(code.as_str(), *wire);
            assert_eq!(BleErrorCode::from_str(wire), Some(*code));
        }
        assert_eq!(BleErrorCode::from_str("nope.unknown"), None);
        assert_eq!(BleErrorCode::from_str(""), None);
    }

    #[test]
    fn core_error_display_and_empty_operation_guard() {
        let error = CoreError::new(
            BleErrorCode::ConnectionStale,
            BleErrorDomain::Connection,
            "kernel.complete",
        );
        assert_eq!(error.code(), BleErrorCode::ConnectionStale);
        assert_eq!(error.domain(), BleErrorDomain::Connection);
        assert_eq!(error.operation(), "kernel.complete");
        assert_eq!(
            format!("{error}"),
            "connection.stale [connection] kernel.complete"
        );
        let guarded = CoreError::new(BleErrorCode::BackendReset, BleErrorDomain::Core, "");
        assert_eq!(guarded.code(), BleErrorCode::ArgumentInvalid);
        assert_eq!(guarded.operation(), "contract-error.operation");
    }

    #[test]
    fn attachment_scope_rejects_foreign_handles() {
        let Some(current) = attachment_fixture() else {
            check(false, "fixture ids must validate");
            return;
        };
        let Some(second) = attachment_fixture() else {
            check(false, "fixture ids must validate");
            return;
        };
        assert!(current.equals(&second));
        let Some(generation) = BackendGeneration::new("bg-4").ok() else {
            check(false, "fixture generation must validate");
            return;
        };
        let Some(foreign_id) = AttachmentId::new("attach-01").ok() else {
            check(false, "fixture ids must validate");
            return;
        };
        let Some(foreign_instance) = BackendInstanceId::new("backend-01").ok() else {
            check(false, "fixture ids must validate");
            return;
        };
        let Some(foreign_adapter) = AdapterId::new("adapter-01").ok() else {
            check(false, "fixture ids must validate");
            return;
        };
        let Some(foreign_adapter_generation) = AdapterGeneration::new("ag-2").ok() else {
            check(false, "fixture ids must validate");
            return;
        };
        let foreign = AttachmentTuple::new(
            foreign_id,
            foreign_instance,
            generation,
            foreign_adapter,
            foreign_adapter_generation,
        );
        assert!(!current.equals(&foreign));
        let rejected = assert_same_attachment(&foreign, &current, "kernel.admit");
        match rejected {
            Err(error) => assert_eq!(error.code(), BleErrorCode::ConnectionStale),
            Ok(()) => check(false, "foreign attachment must be stale"),
        }
        assert!(assert_same_attachment(&current, &current, "kernel.admit").is_ok());
        assert!(AttachmentId::new("").is_err());
        assert!(OperationId::new("op-1").is_ok());
        assert!(LeaseId::new("").is_err());
    }

    #[test]
    fn peer_domains_and_session_keys() {
        assert_eq!(
            PeerIdentityDomain::from_str("public-address"),
            Some(PeerIdentityDomain::PublicAddress)
        );
        assert_eq!(
            PeerIdentityDomain::from_str("opaque-token"),
            Some(PeerIdentityDomain::OpaqueToken)
        );
        assert_eq!(PeerIdentityDomain::from_str("mac"), None);
        assert!(PeerIdentityDomain::PublicAddress.is_globally_stable());
        assert!(PeerIdentityDomain::StaticRandomAddress.is_globally_stable());
        assert!(!PeerIdentityDomain::ResolvablePrivateAddress.is_globally_stable());
        assert!(!PeerIdentityDomain::PlatformGuid.is_globally_stable());
        assert!(!PeerIdentityDomain::OpaqueToken.is_globally_stable());
        let Some(attachment) = attachment_fixture() else {
            check(false, "fixture ids must validate");
            return;
        };
        match PeerIdentity::new(attachment, PeerIdentityDomain::OpaqueToken, "token-9f2") {
            Ok(peer) => assert_eq!(peer.session_key(), "opaque-token:token-9f2"),
            Err(_) => check(false, "peer fixture must validate"),
        }
        let Some(scope) = attachment_fixture() else {
            check(false, "fixture ids must validate");
            return;
        };
        assert!(PeerIdentity::new(scope, PeerIdentityDomain::PlatformGuid, "").is_err());
    }

    #[test]
    fn generations_are_opaque() {
        match (Generation::new("cg-1"), Generation::new("cg-2")) {
            (Ok(first), Ok(second)) => {
                assert!(is_generation_current(&first, &first));
                assert!(!is_generation_current(&first, &second));
            }
            _ => check(false, "generation fixtures must validate"),
        }
    }

    #[test]
    fn capacity_and_timeout_boundaries() {
        assert!(assert_item_capacity(0, "op").is_err());
        assert!(assert_item_capacity(1, "op").is_ok());
        assert!(assert_item_capacity(65_536, "op").is_ok());
        assert!(assert_item_capacity(65_537, "op").is_err());
        assert!(assert_byte_capacity(0, "op").is_err());
        assert!(assert_byte_capacity(4_194_304, "op").is_ok());
        match assert_byte_capacity(4_194_305, "op") {
            Err(error) => assert_eq!(error.code(), BleErrorCode::StreamQuota),
            Ok(_) => check(false, "byte ceiling breach is quota"),
        }
        assert!(assert_timeout_ms(0, "op").is_err());
        assert!(assert_timeout_ms(1, "op").is_ok());
        assert!(assert_timeout_ms(2_147_483_647, "op").is_ok());
        assert!(assert_timeout_ms(2_147_483_648, "op").is_err());
    }

    #[test]
    fn deadlines_are_absolute_and_monotonic() {
        match to_deadline(1_000, 500) {
            Ok(deadline) => {
                assert_eq!(deadline, 1_500);
                assert!(!is_deadline_expired(1_499, deadline));
                assert!(is_deadline_expired(1_500, deadline));
                assert_eq!(earliest_deadline(1_500, 1_200), 1_200);
                assert_eq!(earliest_deadline(1_200, 1_500), 1_200);
            }
            Err(_) => check(false, "deadline fixture must validate"),
        }
        assert!(to_deadline(0, 0).is_err());
        assert!(to_deadline(u64::MAX, 2_147_483_647).is_err());
        assert!(to_deadline(u64::MAX - 10, 10).is_ok());
    }

    #[test]
    fn effective_maxima_take_the_minimum() {
        match effective_max_bytes(&[524_288, 1_024, 4_096]) {
            Ok(maximum) => assert_eq!(maximum, 1_024),
            Err(_) => check(false, "maxima fixture must validate"),
        }
        assert!(effective_max_bytes(&[]).is_err());
        assert!(effective_max_bytes(&[0]).is_err());
        assert!(assert_bytes_within_limit(1_024, &[Some(1_024)], "op").is_ok());
        match assert_bytes_within_limit(1_025, &[Some(1_024)], "op") {
            Err(error) => assert_eq!(error.code(), BleErrorCode::BytesTooLarge),
            Ok(()) => check(false, "oversize payload must fail"),
        }
        match assert_bytes_within_limit(0, &[None], "op") {
            Err(error) => assert_eq!(error.code(), BleErrorCode::CapabilityUnavailable),
            Ok(()) => check(false, "unmeasured maximum is not infinity"),
        }
        assert!(assert_bytes_within_limit(0, &[], "op").is_err());
    }

    #[test]
    fn u64_boundary_fixtures() {
        // Mirrors contracts/src/fixtures/valid.ts + invalid.ts.
        let valid: &[(&str, u64)] = &[
            ("0", 0),
            ("9223372036854775807", 9_223_372_036_854_775_807),
            ("18446744073709551615", u64::MAX),
        ];
        for (input, expected) in valid {
            match parse_u64_decimal(input) {
                Ok(value) => assert_eq!(value, *expected),
                Err(_) => check(false, "valid u64 fixture must parse"),
            }
        }
        let invalid: &[&str] = &[
            "18446744073709551616",
            "-1",
            "1.5",
            "0x10",
            "",
            "+7",
            " 12",
            "12 ",
        ];
        for input in invalid {
            assert!(parse_u64_decimal(input).is_err(), "reject {input}");
        }
    }

    #[test]
    fn i64_boundary_fixtures() {
        let valid: &[(&str, i64)] = &[
            ("-9223372036854775808", i64::MIN),
            ("9223372036854775807", i64::MAX),
            ("-1", -1),
            ("0", 0),
        ];
        for (input, expected) in valid {
            match parse_i64_decimal(input) {
                Ok(value) => assert_eq!(value, *expected),
                Err(_) => check(false, "valid i64 fixture must parse"),
            }
        }
        let invalid: &[&str] = &[
            "9223372036854775808",
            "-9223372036854775809",
            "0x10",
            "1.5",
            "",
            "-",
            "+3",
        ];
        for input in invalid {
            assert!(parse_i64_decimal(input).is_err(), "reject {input}");
        }
    }

    #[test]
    fn terminal_records_hold_exactly_one_outcome() {
        match OperationId::new("op-1") {
            Ok(id) => {
                assert!(
                    TerminalRecord::new(
                        id.clone(),
                        OperationTerminalKind::Succeeded,
                        None,
                        7,
                        100,
                        150
                    )
                    .is_ok()
                );
                assert!(
                    TerminalRecord::new(
                        id.clone(),
                        OperationTerminalKind::Succeeded,
                        Some(BleErrorCode::OperationAborted),
                        7,
                        100,
                        150
                    )
                    .is_err()
                );
                assert!(
                    TerminalRecord::new(
                        id.clone(),
                        OperationTerminalKind::Failed,
                        None,
                        7,
                        100,
                        150
                    )
                    .is_err()
                );
                assert!(
                    TerminalRecord::new(
                        id.clone(),
                        OperationTerminalKind::TimedOut,
                        Some(BleErrorCode::OperationTimedOut),
                        7,
                        100,
                        150
                    )
                    .is_ok()
                );
                assert!(
                    TerminalRecord::new(
                        id,
                        OperationTerminalKind::Aborted,
                        Some(BleErrorCode::OperationAborted),
                        7,
                        150,
                        100
                    )
                    .is_err()
                );
            }
            Err(_) => check(false, "operation id must validate"),
        }
    }

    #[test]
    fn contender_mapping_covers_all_kinds() {
        let cases: &[(ContenderKind, CompletionTerminal, &str)] = &[
            (
                ContenderKind::Success,
                CompletionTerminal::Succeeded,
                "succeeded",
            ),
            (ContenderKind::Failure, CompletionTerminal::Failed, "failed"),
            (
                ContenderKind::DispatchBegin,
                CompletionTerminal::Succeeded,
                "succeeded",
            ),
            (ContenderKind::Abort, CompletionTerminal::Aborted, "aborted"),
            (
                ContenderKind::SessionStop,
                CompletionTerminal::Aborted,
                "aborted",
            ),
            (
                ContenderKind::Timeout,
                CompletionTerminal::TimedOut,
                "timed-out",
            ),
            (
                ContenderKind::Disconnect,
                CompletionTerminal::Disconnected,
                "disconnected",
            ),
            (ContenderKind::Reset, CompletionTerminal::Reset, "reset"),
            (
                ContenderKind::AdapterLoss,
                CompletionTerminal::AdapterUnavailable,
                "adapter-unavailable",
            ),
            (
                ContenderKind::Destroy,
                CompletionTerminal::Destroyed,
                "destroyed",
            ),
        ];
        for (kind, terminal, wire) in cases {
            assert_eq!(terminal_for_winner(*kind), *terminal);
            assert_eq!(terminal.as_str(), *wire);
        }
        assert!(reached_radio_for(true, ContenderKind::Success));
        assert!(!reached_radio_for(true, ContenderKind::Abort));
        assert!(!reached_radio_for(false, ContenderKind::Success));
        assert!(paths_invalid_for(ContenderKind::Disconnect));
        assert!(paths_invalid_for(ContenderKind::Reset));
        assert!(paths_invalid_for(ContenderKind::AdapterLoss));
        assert!(!paths_invalid_for(ContenderKind::Timeout));
        assert_eq!(
            commit_for(true, ContenderKind::Success),
            CommitState::Committed
        );
        assert_eq!(
            commit_for(false, ContenderKind::Abort),
            CommitState::NotDispatched
        );
        assert_eq!(
            commit_for(true, ContenderKind::Timeout),
            CommitState::Unknown
        );
        assert_eq!(
            commit_for(false, ContenderKind::Timeout),
            CommitState::Released
        );
        assert_eq!(
            commit_for(false, ContenderKind::DispatchBegin),
            CommitState::NotDispatched
        );
        assert_eq!(
            commit_for(true, ContenderKind::Destroy),
            CommitState::Released
        );
        assert_eq!(ContenderKind::Success.as_str(), "success");
        assert_eq!(ContenderKind::Failure.as_str(), "failure");
        assert_eq!(ContenderKind::AdapterLoss.as_str(), "adapter-loss");
        assert_eq!(CompletionTerminal::Failed.as_str(), "failed");
        assert_eq!(CommitState::NotDispatched.as_str(), "not-dispatched");
    }

    #[test]
    fn recovery_catalog_covers_every_code() {
        // Exhaustive: adding a code without a recovery entry fails to compile
        // in `recovery_for`; this loop pins representative decisions.
        assert_eq!(
            recovery_for(BleErrorCode::ArgumentInvalid).0,
            RecoveryDisposition::None
        );
        assert!(recovery_for(BleErrorCode::ArgumentInvalid).1.is_empty());
        let (disposition, actions) = recovery_for(BleErrorCode::BytesTooLarge);
        assert_eq!(disposition, RecoveryDisposition::None);
        assert_eq!(actions.len(), 1);
        let (disposition, _) = recovery_for(BleErrorCode::ConnectionStale);
        assert_eq!(disposition, RecoveryDisposition::RetryWithBackoff);
        let (disposition, _) = recovery_for(BleErrorCode::GattStaleHandle);
        assert_eq!(disposition, RecoveryDisposition::RetryImmediately);
        let (disposition, _) = recovery_for(BleErrorCode::StreamQuota);
        assert_eq!(disposition, RecoveryDisposition::RetryWithBackoff);
        let (disposition, actions) = recovery_for(BleErrorCode::PermissionDenied);
        assert_eq!(disposition, RecoveryDisposition::AfterUserAction);
        assert_eq!(actions.len(), 2);
        let all: &[BleErrorCode] = &[
            BleErrorCode::ProtocolIncompatible,
            BleErrorCode::ProtocolMalformed,
            BleErrorCode::ProtocolViolation,
            BleErrorCode::LifecycleDestroyed,
            BleErrorCode::LifecycleInvalidState,
            BleErrorCode::LifecycleInvariantViolation,
            BleErrorCode::BackendReset,
            BleErrorCode::AdapterUnavailable,
            BleErrorCode::AdapterPoweredOff,
            BleErrorCode::AdapterResetting,
            BleErrorCode::AdapterSelectionRequired,
            BleErrorCode::AdapterAmbiguous,
            BleErrorCode::PermissionDenied,
            BleErrorCode::PermissionRestricted,
            BleErrorCode::PermissionNotDetermined,
            BleErrorCode::OwnershipDenied,
            BleErrorCode::ConnectionAlreadyOwned,
            BleErrorCode::ScanAlreadyActive,
            BleErrorCode::ChooserBusy,
            BleErrorCode::ArgumentInvalid,
            BleErrorCode::BytesInvalid,
            BleErrorCode::BytesTooLarge,
            BleErrorCode::ScanStartFailed,
            BleErrorCode::ScanStopFailed,
            BleErrorCode::ScanFilterInvalid,
            BleErrorCode::ChooserCancelled,
            BleErrorCode::ChooserClosed,
            BleErrorCode::ChooserUserActivationRequired,
            BleErrorCode::ChooserInsecureContext,
            BleErrorCode::ChooserApiUnavailable,
            BleErrorCode::ChooserOptionalServiceNotGranted,
            BleErrorCode::ChooserPermittedDeviceUnavailable,
            BleErrorCode::ConnectionNotFound,
            BleErrorCode::ConnectionFailed,
            BleErrorCode::ConnectionStale,
            BleErrorCode::ConnectionLost,
            BleErrorCode::PeerReferenceInvalid,
            BleErrorCode::PeerReferenceVersionUnsupported,
            BleErrorCode::PeerScopeMismatch,
            BleErrorCode::PeerNotFound,
            BleErrorCode::OperationAborted,
            BleErrorCode::OperationTimedOut,
            BleErrorCode::OperationDisconnected,
            BleErrorCode::OperationCancelledByDestroy,
            BleErrorCode::OperationReset,
            BleErrorCode::OperationAdapterUnavailable,
            BleErrorCode::GattDiscoveryRequired,
            BleErrorCode::GattAmbiguousPath,
            BleErrorCode::GattStaleHandle,
            BleErrorCode::GattCacheUnknown,
            BleErrorCode::GattNotFound,
            BleErrorCode::GattPropertyNotSupported,
            BleErrorCode::GattReadFailed,
            BleErrorCode::GattWriteFailed,
            BleErrorCode::GattSubscribeFailed,
            BleErrorCode::GattCccdManaged,
            BleErrorCode::StreamOverflow,
            BleErrorCode::StreamClosed,
            BleErrorCode::StreamQuota,
            BleErrorCode::StreamRateLimited,
            BleErrorCode::CapabilityUnsupported,
            BleErrorCode::CapabilityUnavailable,
            BleErrorCode::CapabilityLimited,
            BleErrorCode::BackgroundTerminated,
            BleErrorCode::PlatformFailure,
            BleErrorCode::PlatformSecurity,
            BleErrorCode::PlatformTransport,
        ];
        assert_eq!(all.len(), 67);
        for code in all {
            let _ = recovery_for(*code);
            assert!(BleErrorCode::from_str(code.as_str()).is_some());
        }
    }

    #[test]
    fn version_negotiation_selects_highest_common() {
        match (
            VersionSpan::new(RuntimeAxis::BackendContract, 1, 3),
            VersionSpan::new(RuntimeAxis::BackendContract, 2, 4),
        ) {
            (Ok(local), Ok(remote)) => match negotiate_version_span(local, remote) {
                Ok(negotiated) => {
                    assert_eq!(negotiated.selected, 3);
                    assert!(assert_negotiated_within_offer(negotiated, local).is_ok());
                    match VersionSpan::new(RuntimeAxis::BackendContract, 4, 4) {
                        Ok(outside) => {
                            assert!(assert_negotiated_within_offer(negotiated, outside).is_err());
                        }
                        Err(_) => check(false, "span fixture must validate"),
                    }
                }
                Err(_) => check(false, "overlapping spans must negotiate"),
            },
            _ => check(false, "span fixtures must validate"),
        }
        assert!(VersionSpan::new(RuntimeAxis::BackendContract, 4, 2).is_err());
        match (
            VersionSpan::new(RuntimeAxis::BackendContract, 1, 1),
            VersionSpan::new(RuntimeAxis::BackendContract, 2, 2),
        ) {
            (Ok(local), Ok(remote)) => {
                assert!(negotiate_version_span(local, remote).is_err());
            }
            _ => check(false, "span fixtures must validate"),
        }
        match (
            VersionSpan::new(RuntimeAxis::BackendContract, 1, 2),
            VersionSpan::new(RuntimeAxis::NativeProtocol, 1, 2),
        ) {
            (Ok(local), Ok(remote)) => {
                assert!(negotiate_version_span(local, remote).is_err());
            }
            _ => check(false, "span fixtures must validate"),
        }
        assert!(assert_contract_revision_equal(CONTRACT_REVISION, CONTRACT_REVISION).is_ok());
        assert!(assert_contract_revision_equal(CONTRACT_REVISION, "C-UBM.0.2.0").is_err());
        assert!(
            assert_handshake_complete(HandshakeState { complete: true }, "kernel.admit").is_ok()
        );
        assert!(
            assert_handshake_complete(HandshakeState { complete: false }, "kernel.admit").is_err()
        );
    }

    #[test]
    fn terminal_kind_wire_strings() {
        assert_eq!(OperationTerminalKind::TimedOut.as_str(), "timed-out");
        assert_eq!(
            OperationTerminalKind::AdapterUnavailable.as_str(),
            "adapter-unavailable"
        );
        assert_eq!(OperationTerminalKind::Destroyed.as_str(), "destroyed");
    }

    #[test]
    fn negotiated_axis_carries_both_offers() {
        let negotiated = NegotiatedAxis {
            axis: RuntimeAxis::EventSchema,
            selected: 2,
            local_minimum: 1,
            local_maximum: 3,
            remote_minimum: 2,
            remote_maximum: 2,
        };
        assert_eq!(negotiated.selected, 2);
        assert_eq!(negotiated.axis, RuntimeAxis::EventSchema);
    }

    #[test]
    fn contract_fix_rulings() {
        use super::{
            GENERIC_PERIPHERAL_ALLOWED_KEYS, GattPath, GattPathParams, MAX_DECIMAL_DIGITS,
            MAX_OPERATION_BYTES, RuntimeAxis, assert_generic_peripheral_decl, effective_max_bytes,
        };
        // R1: a 1MiB declaration clamps to the frozen ceiling.
        match effective_max_bytes(&[1_048_576]) {
            Ok(effective) => assert_eq!(effective, MAX_OPERATION_BYTES),
            Err(_) => check(false, "1MiB max must clamp to the ceiling"),
        }
        assert!(super::assert_bytes_within_limit(524_289, &[Some(1_048_576)], "write").is_err());
        // R4: unknown axes are rejected at the wire boundary.
        assert_eq!(
            RuntimeAxis::from_str("backend-contract"),
            Some(RuntimeAxis::BackendContract)
        );
        assert_eq!(RuntimeAxis::from_str("future-axis"), None);
        assert_eq!(RuntimeAxis::BackendContract.as_str(), "backend-contract");
        // R8: fail-closed generic allowlist.
        assert!(assert_generic_peripheral_decl(&["octetPayload"]).is_ok());
        assert!(assert_generic_peripheral_decl(&["spo2Sample"]).is_err());
        assert!(assert_generic_peripheral_decl(&["sleepStage"]).is_err());
        assert!(GENERIC_PERIPHERAL_ALLOWED_KEYS.contains(&"octetPayload"));
        // R9: paired uuid/occurrence plus attachment scope.
        let attachment = match attachment_fixture() {
            Some(attachment) => attachment,
            None => {
                check(false, "attachment fixture must validate");
                return;
            }
        };
        let peer = match PeerIdentity::new(
            attachment.clone(),
            PeerIdentityDomain::PublicAddress,
            "AA:BB:CC:DD:EE:FF",
        ) {
            Ok(peer) => peer,
            Err(_) => {
                check(false, "peer fixture must validate");
                return;
            }
        };
        let lease = match LeaseId::new("lease-1") {
            Ok(lease) => lease,
            Err(_) => {
                check(false, "lease fixture must validate");
                return;
            }
        };
        let connection_generation = match Generation::new("cg-1") {
            Ok(generation) => generation,
            Err(_) => {
                check(false, "generation fixture must validate");
                return;
            }
        };
        let database_generation = match Generation::new("dg-1") {
            Ok(generation) => generation,
            Err(_) => {
                check(false, "generation fixture must validate");
                return;
            }
        };
        assert!(
            GattPath::new(GattPathParams {
                attachment: attachment.clone(),
                peer: peer.clone(),
                connection_generation: connection_generation.clone(),
                database_generation: database_generation.clone(),
                service_uuid: String::from("180D"),
                service_occurrence: 0,
                characteristic_uuid: Some(String::from("2A37")),
                characteristic_occurrence: None,
                descriptor_uuid: None,
                descriptor_occurrence: None,
                owner_lease: lease.clone(),
            })
            .is_err()
        );
        assert!(
            GattPath::new(GattPathParams {
                attachment: attachment.clone(),
                peer: peer.clone(),
                connection_generation: connection_generation.clone(),
                database_generation: database_generation.clone(),
                service_uuid: String::from("180D"),
                service_occurrence: 0,
                characteristic_uuid: None,
                characteristic_occurrence: None,
                descriptor_uuid: Some(String::from("2902")),
                descriptor_occurrence: Some(0),
                owner_lease: lease.clone(),
            })
            .is_err()
        );
        // R14: canonical decimal cap.
        assert_eq!(MAX_DECIMAL_DIGITS, 20);
        assert!(super::parse_u64_decimal("+1").is_err());
        assert!(super::parse_u64_decimal(" 1").is_err());
        assert!(super::parse_i64_decimal("+1").is_err());
        assert!(super::parse_u64_decimal("111111111111111111111").is_err());
    }
}
