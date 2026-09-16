//! Contract error identities for the desktop central (HOST-DESKTOP).
//!
//! Every failure the desktop adapter surfaces carries a frozen C-UBM error
//! identity ([`BleErrorCode`] plus [`BleErrorDomain`]); the radio boundary
//! never invents codes. [`DesktopError`] preserves a [`CoreError`] identity
//! verbatim through `From`, and dedicated constructors attribute btleplug
//! boundary failures to their contract operations.

use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CoreError};

/// Host-side error carrying a frozen contract identity plus an optional
/// transport detail (kept out of the identity triple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopError {
    code: BleErrorCode,
    domain: BleErrorDomain,
    operation: String,
    detail: Option<String>,
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
            };
        }
        Self {
            code,
            domain,
            operation,
            detail: None,
        }
    }

    /// Attach a transport detail without changing the identity triple.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
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
    use ubm_core::contracts::{BleErrorCode, BleErrorDomain, CoreError};

    use super::DesktopError;

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
    fn empty_operation_fails_closed() {
        let error = DesktopError::new(BleErrorCode::BackendReset, BleErrorDomain::Core, "");
        assert_eq!(error.code(), BleErrorCode::ArgumentInvalid);
    }
}
