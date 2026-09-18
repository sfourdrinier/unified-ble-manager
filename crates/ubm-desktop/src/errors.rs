//! Contract error identities for the desktop central (HOST-DESKTOP).
//!
//! Every failure the desktop adapter surfaces carries a frozen C-UBM error
//! identity ([`BleErrorCode`] plus [`BleErrorDomain`]); the radio boundary
//! never invents codes. [`DesktopError`] preserves a [`CoreError`] identity
//! verbatim through `From`, and dedicated constructors attribute btleplug
//! boundary failures to their contract operations.

use std::collections::BTreeMap;

use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState, CoreError};

/// One typed platform metadata value (finding 113).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformValue {
    Int(i64),
    Text(String),
    Bool(bool),
}

/// The platform's own answer behind an error, as typed fields rather than
/// free text (finding 113), so a host restores the legacy error identity:
///
/// - CoreBluetooth: `{domain:"corebluetooth", code:<NSError code>}`;
/// - WinRT: `{domain:"winrt", code, metadata:{hresult, gattStatus}}`;
/// - BlueZ: `{domain:"bluez-dbus", code:<D-Bus error name>}`;
/// - Android: `{domain:"android", code, metadata:{androidGattStatus}}`.
///
/// Never part of the contract identity triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformDetail {
    pub domain: String,
    pub code: String,
    pub message: Option<String>,
    pub metadata: BTreeMap<String, PlatformValue>,
}

impl PlatformDetail {
    /// A detail with no message or metadata.
    #[must_use]
    pub fn new(domain: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            code: code.into(),
            message: None,
            metadata: BTreeMap::new(),
        }
    }

    /// This detail with the platform's message.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// This detail with one metadata entry.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: PlatformValue) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// Android GATT statuses a connect fails with when the link could not be
/// established: 133 `GATT_ERROR` (the stack's generic connect failure),
/// 62 (HCI 0x3E, "connection failed to be established") and 147
/// `GATT_CONNECTION_TIMEOUT` (API 34).
const ANDROID_TRANSIENT_CONNECT_STATUSES: [i64; 3] = [133, 62, 147];

/// `CBError.connectionTimeout` (6) and `CBError.connectionFailed` (10).
const COREBLUETOOTH_TRANSIENT_CONNECT_CODES: [&str; 2] = ["6", "10"];

/// BlueZ `Device1.Connect` failures that mean the link attempt failed
/// (`org.bluez.Error.Failed` carries the reason, e.g.
/// `le-connection-abort-by-local`).
const BLUEZ_TRANSIENT_CONNECT_ERRORS: [&str; 2] = [
    "org.bluez.Error.Failed",
    "org.bluez.Error.ConnectionAttemptFailed",
];

/// Whether the platform's answer to a connect is a transient failure to
/// establish the link — one vocabulary for every platform:
///
/// - Android: `androidGattStatus` 133, 62 (0x3E) or 147;
/// - CoreBluetooth: `CBErrorDomain` 6 or 10, as the mobile radio reports it
///   (`{domain:"CBErrorDomain"}`) and as the desktop radio does
///   (`{domain:"corebluetooth", metadata:{nsErrorDomain:"CBErrorDomain"}}`);
/// - WinRT: `gatt-status` `unreachable` (`GetGattServicesAsync` could not
///   reach the device);
/// - BlueZ: `org.bluez.Error.Failed` or `ConnectionAttemptFailed`.
#[must_use]
pub fn is_transient_establishment_failure(platform: &PlatformDetail) -> bool {
    let text = |key: &str| match platform.metadata.get(key) {
        Some(PlatformValue::Text(value)) => Some(value.as_str()),
        _ => None,
    };
    match platform.domain.as_str() {
        "android" => matches!(
            platform.metadata.get("androidGattStatus"),
            Some(PlatformValue::Int(status)) if ANDROID_TRANSIENT_CONNECT_STATUSES.contains(status)
        ),
        "CBErrorDomain" => COREBLUETOOTH_TRANSIENT_CONNECT_CODES.contains(&platform.code.as_str()),
        "corebluetooth" => {
            text("nsErrorDomain") == Some("CBErrorDomain")
                && COREBLUETOOTH_TRANSIENT_CONNECT_CODES.contains(&platform.code.as_str())
        }
        "winrt" => platform.code == "gatt-status" && text("gattStatus") == Some("unreachable"),
        "bluez-dbus" => BLUEZ_TRANSIENT_CONNECT_ERRORS.contains(&platform.code.as_str()),
        _ => false,
    }
}

/// Whether the caller may safely repeat the operation (PR210-22). Set by
/// the central from the core's settled outcome, never derived from the
/// error code: a write that may have reached the peer is `Never`, whatever
/// its code says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    /// Repeating may duplicate an effect, or the failure is not transient.
    Never,
    /// The operation left no uncertain effect; the caller may repeat it.
    CallerDecides,
}

impl Retryability {
    /// Frozen wire string (`never` / `caller-decides`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::CallerDecides => "caller-decides",
        }
    }
}

/// Host-side error carrying a frozen contract identity plus an optional
/// transport detail (kept out of the identity triple), and the outcome
/// facts the central observed: the commit state of the operation, when
/// known, and whether the caller may retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopError {
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: String,
    detail: Option<String>,
    /// Boxed so the error stays small on every `Result` path.
    platform: Option<Box<PlatformDetail>>,
    commit: Option<CommitState>,
    retryability: Retryability,
}

impl DesktopError {
    /// Build an error with a frozen identity. An empty operation is itself
    /// `argument.invalid`, mirroring [`CoreError`].
    pub fn new(code: BleErrorCode, domain: BleErrorDomain, operation: impl Into<String>) -> Self {
        let operation = operation.into();
        if operation.is_empty() {
            return Self {
                code: BleErrorCode::ArgumentInvalid,
                domain: BleErrorDomain::Core,
                operation: String::from("contract-error.operation"),
                detail: None,
                platform: None,
                commit: None,
                retryability: Retryability::Never,
            };
        }
        Self {
            code,
            domain,
            operation,
            detail: None,
            platform: None,
            commit: None,
            retryability: Retryability::Never,
        }
    }

    /// Record the settled outcome facts without changing the identity
    /// triple: the commit state (when known) and the retryability the
    /// central derived from it.
    #[must_use]
    pub fn with_outcome(mut self, commit: Option<CommitState>, retryability: Retryability) -> Self {
        self.commit = commit;
        self.retryability = retryability;
        self
    }

    /// Owner decision (5.0): a connect whose link the platform could not
    /// establish, transiently, is `caller-decides` — nothing was committed,
    /// so repeating it is the caller's policy. The library never retries it
    /// itself. The platform's answer decides ([`is_transient_establishment_failure`]);
    /// without one, or for any other operation, the error is unchanged.
    #[must_use]
    pub fn classify_establishment(self) -> Self {
        let transient = self.operation == "connection.connect"
            && self
                .platform()
                .is_some_and(is_transient_establishment_failure);
        if transient {
            let commit = self.commit;
            self.with_outcome(commit, Retryability::CallerDecides)
        } else {
            self
        }
    }

    /// Commit state of the operation, when the central knows it
    /// (`not-dispatched` before any radio call, `unknown` for a write that
    /// may have reached the peer).
    #[must_use]
    pub const fn commit(&self) -> Option<CommitState> {
        self.commit
    }

    /// Whether the caller may repeat the operation. Defaults to `Never`.
    #[must_use]
    pub const fn retryability(&self) -> Retryability {
        self.retryability
    }

    /// Attach a transport detail without changing the identity triple.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach the platform's structured answer (finding 113) without
    /// changing the identity triple.
    #[must_use]
    pub fn with_platform(mut self, platform: PlatformDetail) -> Self {
        self.platform = Some(Box::new(platform));
        self
    }

    /// The platform's structured answer, if the platform gave one.
    #[must_use]
    pub fn platform(&self) -> Option<&PlatformDetail> {
        self.platform.as_deref()
    }

    /// Frozen error code.
    #[must_use]
    pub const fn code(&self) -> BleErrorCode {
        self.code
    }

    /// Frozen wire string of the code (e.g. `"scan.start-failed"`).
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
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

    /// Transport detail, if any. Never part of the contract identity.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Adapter unavailable (`adapter.unavailable`, adapter domain).
    pub fn adapter_unavailable(operation: &str) -> Self {
        Self::new(
            BleErrorCode::AdapterUnavailable,
            BleErrorDomain::Adapter,
            operation,
        )
    }

    /// OS radio rejected scan start (`scan.start-failed`, scan domain).
    pub fn scan_start_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::ScanStartFailed,
            BleErrorDomain::Scan,
            "scan.start",
        )
        .with_detail(detail)
    }

    /// OS radio rejected scan stop (`scan.stop-failed`, scan domain). Stopping
    /// still settles the core session as failed; the error reports the
    /// cleanup outcome truthfully instead of swallowing it.
    pub fn scan_stop_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::ScanStopFailed,
            BleErrorDomain::Scan,
            "scan.stop",
        )
        .with_detail(detail)
    }

    /// OS radio rejected connect (`connection.failed`, connection domain).
    pub fn connection_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::ConnectionFailed,
            BleErrorDomain::Connection,
            "connection.connect",
        )
        .with_detail(detail)
    }

    /// GATT read rejected by the radio (`gatt.read-failed`, GATT domain).
    pub fn read_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::GattReadFailed,
            BleErrorDomain::Gatt,
            "gatt.read",
        )
        .with_detail(detail)
    }

    /// GATT write rejected by the radio (`gatt.write-failed`, GATT domain).
    pub fn write_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::GattWriteFailed,
            BleErrorDomain::Gatt,
            "gatt.write",
        )
        .with_detail(detail)
    }

    /// Notification enablement rejected by the radio
    /// (`gatt.subscribe-failed`, GATT domain).
    pub fn subscribe_failed(detail: impl Into<String>) -> Self {
        Self::new(
            BleErrorCode::GattSubscribeFailed,
            BleErrorDomain::Gatt,
            "gatt.subscribe",
        )
        .with_detail(detail)
    }

    /// Caller cancelled the operation (`operation.aborted`, connection
    /// domain): the radio op never dispatched, so the core op is cancelled,
    /// not settled as a radio failure.
    pub fn cancelled(operation: &str) -> Self {
        Self::new(
            BleErrorCode::OperationAborted,
            BleErrorDomain::Connection,
            operation,
        )
    }
}

impl From<CoreError> for DesktopError {
    /// Preserve the core identity verbatim: code, domain, and operation path
    /// cross the boundary unchanged.
    fn from(error: CoreError) -> Self {
        Self {
            code: error.code(),
            domain: error.domain(),
            operation: error.operation().to_owned(),
            detail: None,
            platform: None,
            commit: None,
            retryability: Retryability::Never,
        }
    }
}

impl core::fmt::Display for DesktopError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({})", self.code.as_str(), self.operation)
    }
}

impl std::error::Error for DesktopError {}

#[cfg(test)]
mod tests {

    /// Finding 113: the platform's structured answer rides the error
    /// through outcome classification unchanged and never alters the
    /// identity triple.
    #[test]
    fn the_platform_detail_survives_classification() {
        use super::{PlatformDetail, PlatformValue, Retryability};
        let platform = PlatformDetail::new("winrt", "0x80650005")
            .with_message("protocol error")
            .with_metadata("hresult", PlatformValue::Int(0x8065_0005))
            .with_metadata("gattStatus", PlatformValue::Int(5));
        let error = super::DesktopError::read_failed("detail")
            .with_platform(platform.clone())
            .with_outcome(None, Retryability::CallerDecides)
            .with_detail("more");
        assert_eq!(error.platform(), Some(&platform));
        assert_eq!(error.code_str(), "gatt.read-failed");
        assert_eq!(super::DesktopError::read_failed("x").platform(), None);
    }
    use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CommitState, CoreError};

    use super::{DesktopError, Retryability};

    #[test]
    fn core_identity_crosses_verbatim() {
        let core = CoreError::new(
            BleErrorCode::ScanAlreadyActive,
            BleErrorDomain::Core,
            "scan.arbitration",
        );
        let error = DesktopError::from(core);
        assert_eq!(error.code(), BleErrorCode::ScanAlreadyActive);
        assert_eq!(error.domain(), BleErrorDomain::Core);
        assert_eq!(error.operation(), "scan.arbitration");
        assert_eq!(error.code_str(), "scan.already-active");
        assert_eq!(error.detail(), None);
    }

    #[test]
    fn boundary_failures_carry_contract_identities() {
        let cases = [
            (
                DesktopError::scan_start_failed("denied"),
                "scan.start-failed",
                "scan.start",
            ),
            (
                DesktopError::scan_stop_failed("gone"),
                "scan.stop-failed",
                "scan.stop",
            ),
            (
                DesktopError::connection_failed("timeout"),
                "connection.failed",
                "connection.connect",
            ),
            (
                DesktopError::read_failed("att"),
                "gatt.read-failed",
                "gatt.read",
            ),
            (
                DesktopError::write_failed("att"),
                "gatt.write-failed",
                "gatt.write",
            ),
            (
                DesktopError::subscribe_failed("cccd"),
                "gatt.subscribe-failed",
                "gatt.subscribe",
            ),
        ];
        for (error, code, operation) in cases {
            assert_eq!(error.code_str(), code, "code for {operation}");
            assert_eq!(error.operation(), operation);
            assert!(error.detail().is_some(), "detail kept for {operation}");
        }
    }

    #[test]
    fn errors_default_to_never_retryable_without_a_commit_fact() {
        let error = DesktopError::write_failed("att");
        assert_eq!(error.retryability(), Retryability::Never);
        assert_eq!(error.commit(), None);
        let from_core = DesktopError::from(CoreError::new(
            BleErrorCode::OperationTimedOut,
            BleErrorDomain::Connection,
            "gatt.read",
        ));
        assert_eq!(
            from_core.retryability(),
            Retryability::Never,
            "a code alone never makes an error retryable"
        );
    }

    #[test]
    fn outcome_facts_travel_with_the_error_but_not_its_identity() {
        let error = DesktopError::cancelled("gatt.write").with_outcome(
            Some(CommitState::NotDispatched),
            Retryability::CallerDecides,
        );
        assert_eq!(error.commit(), Some(CommitState::NotDispatched));
        assert_eq!(error.retryability(), Retryability::CallerDecides);
        assert_eq!(error.code_str(), "operation.aborted");
        assert_eq!(Retryability::CallerDecides.as_str(), "caller-decides");
        assert_eq!(Retryability::Never.as_str(), "never");
    }

    /// Owner decision (5.0): a connect the platform could not establish is
    /// `caller-decides` on every platform, with the platform's own answer
    /// kept; every other connect failure, and any other operation, stays
    /// `never`.
    #[test]
    fn a_transient_link_establishment_failure_is_caller_decides() {
        use super::{PlatformDetail, PlatformValue};
        let android = |status: i64| {
            PlatformDetail::new("android", "connectionFailed")
                .with_metadata("androidGattStatus", PlatformValue::Int(status))
        };
        let desktop_cb = |code: &str| {
            PlatformDetail::new("corebluetooth", code)
                .with_metadata("nsErrorDomain", PlatformValue::Text("CBErrorDomain".to_owned()))
        };
        let winrt = |status: &str| {
            PlatformDetail::new("winrt", "gatt-status")
                .with_metadata("gattStatus", PlatformValue::Text(status.to_owned()))
        };
        let transient = [
            android(133),
            android(62),
            android(147),
            PlatformDetail::new("CBErrorDomain", "6"),
            PlatformDetail::new("CBErrorDomain", "10"),
            desktop_cb("6"),
            desktop_cb("10"),
            winrt("unreachable"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("le-connection-abort-by-local"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.ConnectionAttemptFailed"),
        ];
        let permanent = [
            android(5),
            android(0),
            PlatformDetail::new("android", "connectionFailed"),
            PlatformDetail::new("CBErrorDomain", "14"),
            PlatformDetail::new("corebluetooth", "10"),
            desktop_cb("14"),
            winrt("access-denied"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.InProgress"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotReady"),
        ];
        let connect = |platform: &PlatformDetail| {
            DesktopError::new(
                BleErrorCode::PlatformFailure,
                BleErrorDomain::Platform,
                "connection.connect",
            )
            .with_platform(platform.clone())
            .classify_establishment()
        };
        for platform in &transient {
            let error = connect(platform);
            assert_eq!(
                error.retryability(),
                Retryability::CallerDecides,
                "{platform:?} is a transient establishment failure"
            );
            assert_eq!(error.platform(), Some(platform), "platform detail kept");
            assert_eq!(error.commit(), None);
        }
        for platform in &permanent {
            assert_eq!(
                connect(platform).retryability(),
                Retryability::Never,
                "{platform:?} is not"
            );
        }
        let discover = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "gatt.discover",
        )
        .with_platform(android(133))
        .classify_establishment();
        assert_eq!(discover.retryability(), Retryability::Never, "connect only");
        assert_eq!(
            DesktopError::connection_failed("no platform answer")
                .classify_establishment()
                .retryability(),
            Retryability::Never,
            "no platform answer, no second opinion"
        );
    }

    #[test]
    fn empty_operation_fails_closed() {
        let error = DesktopError::new(BleErrorCode::BackendReset, BleErrorDomain::Core, "");
        assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
    }
}
