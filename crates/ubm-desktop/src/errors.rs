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

/// Whether the platform's answer to an operation on a link says the link is
/// gone — one vocabulary for every desktop platform, the counterpart of
/// Android's `not-connected`:
///
/// - CoreBluetooth: `CBErrorDomain` 3 (`notConnected`) or 7
///   (`peripheralDisconnected`);
/// - WinRT: `gatt-status` `unreachable`;
/// - BlueZ: `org.bluez.Error.NotConnected`, or `org.bluez.Error.Failed`
///   with the message `Not connected`;
/// - btleplug's own `NotConnected` (`{domain:"btleplug", code:"not-connected"}`).
#[must_use]
pub fn is_link_loss_answer(platform: &PlatformDetail) -> bool {
    let text = |key: &str| match platform.metadata.get(key) {
        Some(PlatformValue::Text(value)) => Some(value.as_str()),
        _ => None,
    };
    match platform.domain.as_str() {
        "corebluetooth" => {
            text("nsErrorDomain") == Some("CBErrorDomain")
                && matches!(platform.code.as_str(), "3" | "7")
        }
        "winrt" => platform.code == "gatt-status" && text("gattStatus") == Some("unreachable"),
        "bluez-dbus" => {
            platform.code == "org.bluez.Error.NotConnected"
                || (platform.code == "org.bluez.Error.Failed"
                    && platform.message.as_deref() == Some("Not connected"))
        }
        "btleplug" => platform.code == "not-connected",
        _ => false,
    }
}

/// Android GATT statuses of a refusal for lack of security: 5
/// `INSUFFICIENT_AUTHENTICATION`, 8 `INSUFFICIENT_AUTHORIZATION`, 12
/// insufficient encryption key size, 15 `INSUFFICIENT_ENCRYPTION`, 137
/// (`0x89`, the stack's authentication failure).
const ANDROID_SECURITY_STATUSES: [i64; 5] = [5, 8, 12, 15, 137];

/// The same ATT errors as CoreBluetooth's `CBATTErrorDomain` codes.
const ATT_SECURITY_CODES: [&str; 4] = ["5", "8", "12", "15"];

/// `CBError.peerRemovedPairingInformation` (14), `encryptionTimedOut` (15).
const COREBLUETOOTH_SECURITY_CODES: [&str; 2] = ["14", "15"];

/// Whether the platform's answer is a refusal for lack of authentication,
/// authorization or encryption — one vocabulary for every host that can
/// tell (`platform.security`, recovery pair / repair):
///
/// - Android: `androidGattStatus` 5, 8, 12, 15 or 137;
/// - CoreBluetooth: `CBATTErrorDomain` 5, 8, 12 or 15, `CBErrorDomain` 14 or
///   15 (mobile `{domain:<NSError domain>}`, desktop
///   `{domain:"corebluetooth", metadata:{nsErrorDomain}}`);
/// - BlueZ: `org.bluez.Error.NotAuthorized`, `AuthenticationFailed`, or
///   `NotPermitted` with the message `Not paired`.
///
/// WinRT's `GattCommunicationStatus` `ProtocolError` does not carry the ATT
/// error through the radio, so a Windows refusal cannot be told apart and
/// keeps its GATT code.
#[must_use]
pub fn is_security_answer(platform: &PlatformDetail) -> bool {
    let text = |key: &str| match platform.metadata.get(key) {
        Some(PlatformValue::Text(value)) => Some(value.as_str()),
        _ => None,
    };
    let apple = |domain: Option<&str>, code: &str| match domain {
        Some("CBATTErrorDomain") => ATT_SECURITY_CODES.contains(&code),
        Some("CBErrorDomain") => COREBLUETOOTH_SECURITY_CODES.contains(&code),
        _ => false,
    };
    match platform.domain.as_str() {
        "android" => matches!(
            platform.metadata.get("androidGattStatus"),
            Some(PlatformValue::Int(status)) if ANDROID_SECURITY_STATUSES.contains(status)
        ),
        "CBATTErrorDomain" | "CBErrorDomain" => {
            apple(Some(platform.domain.as_str()), &platform.code)
        }
        "corebluetooth" => apple(text("nsErrorDomain"), &platform.code),
        "bluez-dbus" => {
            matches!(
                platform.code.as_str(),
                "org.bluez.Error.NotAuthorized" | "org.bluez.Error.AuthenticationFailed"
            ) || (platform.code == "org.bluez.Error.NotPermitted"
                && platform.message.as_deref() == Some("Not paired"))
        }
        _ => false,
    }
}

/// Operations that run on an established link: GATT verbs and discovery.
/// A connect, a disconnect, a scan or an adapter read is not one.
fn is_link_operation(operation: &str) -> bool {
    operation.starts_with("gatt.") || operation.starts_with("discovery.")
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

    /// Owner decision (5.0): an operation on a link the platform says is
    /// gone ([`is_link_loss_answer`]) reports `connection.lost` on every
    /// host, as Android does, with the platform's answer and the detail
    /// kept. Any other error, or a non-link operation, is unchanged.
    #[must_use]
    pub fn classify_link_loss(mut self) -> Self {
        let lost =
            is_link_operation(&self.operation) && self.platform().is_some_and(is_link_loss_answer);
        if lost {
            self.code = BleErrorCode::ConnectionLost;
            self.domain = BleErrorDomain::Connection;
        }
        self
    }

    /// Owner decision (5.0): a connect the platform failed is
    /// `connection.failed` on every host, the platform's answer kept; a
    /// more specific code (permission, adapter, peer, cancel, timeout) is
    /// unchanged.
    #[must_use]
    pub fn classify_connect_failure(mut self) -> Self {
        if self.operation == "connection.connect" && self.code == BleErrorCode::PlatformFailure {
            self.code = BleErrorCode::ConnectionFailed;
            self.domain = BleErrorDomain::Connection;
        }
        self
    }

    /// Owner decision (5.0): a link operation the peer refused for lack of
    /// security ([`is_security_answer`]) is `platform.security` on every
    /// host that can tell, the platform's answer and detail kept.
    #[must_use]
    pub fn classify_security(mut self) -> Self {
        // Only a generic refusal is renamed: a link loss that carries the
        // same number (Android status 8 is both HCI connection timeout and
        // ATT insufficient authorization) keeps its identity.
        let generic = matches!(
            self.code,
            BleErrorCode::PlatformFailure
                | BleErrorCode::GattReadFailed
                | BleErrorCode::GattWriteFailed
                | BleErrorCode::GattSubscribeFailed
        );
        let refused = generic
            && is_link_operation(&self.operation)
            && self.platform().is_some_and(is_security_answer);
        if refused {
            self.code = BleErrorCode::PlatformSecurity;
            self.domain = BleErrorDomain::Platform;
        }
        self
    }

    /// Owner decision (5.0): a link operation cut off by the app's own
    /// release is `operation.disconnected`; the same failure while the link
    /// was not being released stays `connection.lost`. The central calls
    /// this with the connection state it observed when the operation
    /// settled.
    #[must_use]
    pub fn named_for_requested_release(mut self, release_requested: bool) -> Self {
        if release_requested && self.code == BleErrorCode::ConnectionLost {
            self.code = BleErrorCode::OperationDisconnected;
        }
        self
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
            PlatformDetail::new("corebluetooth", code).with_metadata(
                "nsErrorDomain",
                PlatformValue::Text("CBErrorDomain".to_owned()),
            )
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

    /// Owner decision (5.0): an operation on a link that the platform says
    /// is gone reports `connection.lost` on every host, as Android does,
    /// with the platform's answer kept. Only a link operation is converted;
    /// a connect, a disconnect or a scan keeps its own identity.
    #[test]
    fn a_link_loss_answer_on_a_link_operation_is_connection_lost() {
        use super::{PlatformDetail, PlatformValue};
        let cb = |code: &str| {
            PlatformDetail::new("corebluetooth", code).with_metadata(
                "nsErrorDomain",
                PlatformValue::Text("CBErrorDomain".to_owned()),
            )
        };
        let lost = [
            cb("3"),
            cb("7"),
            PlatformDetail::new("winrt", "gatt-status")
                .with_metadata("gattStatus", PlatformValue::Text("unreachable".to_owned())),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotConnected"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("Not connected"),
            PlatformDetail::new("btleplug", "not-connected"),
        ];
        let other = [
            cb("10"),
            PlatformDetail::new("corebluetooth", "7"),
            PlatformDetail::new("winrt", "gatt-status").with_metadata(
                "gattStatus",
                PlatformValue::Text("protocol-error".to_owned()),
            ),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.Failed")
                .with_message("Operation failed"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted"),
        ];
        for platform in &lost {
            for operation in [
                "gatt.read",
                "gatt.discover",
                "discovery.complete",
                "gatt.subscribe",
            ] {
                let error = DesktopError::new(
                    BleErrorCode::GattReadFailed,
                    BleErrorDomain::Gatt,
                    operation,
                )
                .with_detail("radio said so")
                .with_platform(platform.clone())
                .classify_link_loss();
                assert_eq!(
                    error.code(),
                    BleErrorCode::ConnectionLost,
                    "{platform:?} {operation}"
                );
                assert_eq!(error.domain(), BleErrorDomain::Connection);
                assert_eq!(error.operation(), operation);
                assert_eq!(
                    error.platform(),
                    Some(platform),
                    "the platform's answer is kept"
                );
                assert_eq!(error.detail(), Some("radio said so"));
            }
            for operation in [
                "connection.connect",
                "connection.disconnect",
                "scan.start",
                "peer.list",
            ] {
                let error = DesktopError::new(
                    BleErrorCode::PlatformFailure,
                    BleErrorDomain::Platform,
                    operation,
                )
                .with_platform(platform.clone())
                .classify_link_loss();
                assert_eq!(
                    error.code(),
                    BleErrorCode::PlatformFailure,
                    "{operation} keeps its identity"
                );
            }
        }
        for platform in &other {
            let error = DesktopError::read_failed("x")
                .with_platform(platform.clone())
                .classify_link_loss();
            assert_eq!(error.code(), BleErrorCode::GattReadFailed, "{platform:?}");
        }
        assert_eq!(
            DesktopError::read_failed("no answer")
                .classify_link_loss()
                .code(),
            BleErrorCode::GattReadFailed,
            "no platform answer, no second opinion"
        );
    }

    /// Owner decision (5.0): a peer that refuses an operation for lack of
    /// authentication, authorization or encryption is `platform.security`
    /// (recovery: pair or repair) on every host that can tell, the
    /// platform's answer kept. Other refusals keep their code.
    #[test]
    fn an_authentication_or_encryption_refusal_is_platform_security() {
        use super::{PlatformDetail, PlatformValue};
        let android = |status: i64| {
            PlatformDetail::new("android", "readFailed")
                .with_metadata("androidGattStatus", PlatformValue::Int(status))
        };
        let desktop_att = |code: &str| {
            PlatformDetail::new("corebluetooth", code).with_metadata(
                "nsErrorDomain",
                PlatformValue::Text("CBATTErrorDomain".to_owned()),
            )
        };
        let security = [
            android(5),
            android(8),
            android(12),
            android(15),
            android(137),
            PlatformDetail::new("CBATTErrorDomain", "5"),
            PlatformDetail::new("CBATTErrorDomain", "15"),
            PlatformDetail::new("CBErrorDomain", "14"),
            PlatformDetail::new("CBErrorDomain", "15"),
            desktop_att("5"),
            desktop_att("8"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotAuthorized"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted")
                .with_message("Not paired"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.AuthenticationFailed"),
        ];
        let other = [
            android(3),
            android(133),
            PlatformDetail::new("CBATTErrorDomain", "3"),
            desktop_att("3"),
            PlatformDetail::new("bluez-dbus", "org.bluez.Error.NotPermitted")
                .with_message("Read not permitted"),
            PlatformDetail::new("winrt", "gatt-status").with_metadata(
                "gattStatus",
                PlatformValue::Text("protocol-error".to_owned()),
            ),
        ];
        for platform in &security {
            let error = DesktopError::read_failed("refused")
                .with_platform(platform.clone())
                .classify_security();
            assert_eq!(error.code(), BleErrorCode::PlatformSecurity, "{platform:?}");
            assert_eq!(error.domain(), BleErrorDomain::Platform);
            assert_eq!(error.platform(), Some(platform));
        }
        for platform in &other {
            let error = DesktopError::read_failed("refused")
                .with_platform(platform.clone())
                .classify_security();
            assert_eq!(error.code(), BleErrorCode::GattReadFailed, "{platform:?}");
        }
        let connect = DesktopError::connection_failed("x")
            .with_platform(android(5))
            .classify_security();
        assert_eq!(
            connect.code(),
            BleErrorCode::ConnectionFailed,
            "link operations only"
        );
        let lost = DesktopError::new(
            BleErrorCode::ConnectionLost,
            BleErrorDomain::Connection,
            "gatt.read",
        )
        .with_platform(android(8))
        .classify_security();
        assert_eq!(
            lost.code(),
            BleErrorCode::ConnectionLost,
            "Android status 8 at a disconnect is a link loss, not a refusal"
        );
    }

    /// Owner decision (5.0): a connect the platform failed is
    /// `connection.failed` on every host (Android and BlueZ reported
    /// `platform.failure`, CoreBluetooth, WinRT and Web `connection.failed`),
    /// the platform's answer kept. Codes that name something more specific
    /// (permission, adapter, peer, cancel, timeout) are unchanged.
    #[test]
    fn a_platform_connect_failure_is_connection_failed() {
        use super::{PlatformDetail, PlatformValue};
        let android = PlatformDetail::new("android", "connectionFailed")
            .with_metadata("androidGattStatus", PlatformValue::Int(133));
        let error = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "connection.connect",
        )
        .with_platform(android.clone())
        .classify_connect_failure();
        assert_eq!(error.code(), BleErrorCode::ConnectionFailed);
        assert_eq!(error.domain(), BleErrorDomain::Connection);
        assert_eq!(error.platform(), Some(&android));
        for code in [
            BleErrorCode::PermissionDenied,
            BleErrorCode::AdapterPoweredOff,
            BleErrorCode::PeerNotFound,
            BleErrorCode::OperationTimedOut,
        ] {
            let kept = DesktopError::new(code, BleErrorDomain::Connection, "connection.connect")
                .classify_connect_failure();
            assert_eq!(kept.code(), code);
        }
        let read = DesktopError::new(
            BleErrorCode::PlatformFailure,
            BleErrorDomain::Platform,
            "gatt.read",
        )
        .classify_connect_failure();
        assert_eq!(read.code(), BleErrorCode::PlatformFailure, "connect only");
    }

    #[test]
    fn empty_operation_fails_closed() {
        let error = DesktopError::new(BleErrorCode::BackendReset, BleErrorDomain::Core, "");
        assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
    }
}
