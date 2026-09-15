//! Core-backed binding core for the JNI binding (no Tokio, no filesystem,
//! no radio).
//!
//! The `CoreBackend` seam is implemented for [`CoreSession`], whose contract
//! truth is single-owned by `ubm-core` (frozen `C-UBM.0.1.1-DRAFT`): the
//! revision identity, the byte ceiling, and the decimal-string counter
//! parsing all come from `ubm_core::contracts`. No contract constant or
//! validator is duplicated here — the previous echo-only stand-in
//! (`echo_core.rs`) is deleted, so there are no dual owners.
//!
//! The echo transport itself stays feasibility-echo (NOT BLE functionality);
//! wiring real kernel transitions through this seam is later U7 scope.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Frozen contract revision, single-owned by `ubm-core`.
pub use ubm_core::contracts::CONTRACT_REVISION;

/// Typed failure carrying a frozen C-UBM `code` + `domain` plus the operation
/// under test. Never silent: every rejection names its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoError {
    pub code: &'static str,
    pub domain: &'static str,
    pub operation: &'static str,
    pub detail: &'static str,
}

impl EchoError {
    pub const fn new(
        code: &'static str,
        domain: &'static str,
        operation: &'static str,
        detail: &'static str,
    ) -> Self {
        Self {
            code,
            domain,
            operation,
            detail,
        }
    }

    /// Wire form shared by every binding: `code|domain|operation|detail`.
    /// The JNI boundary throws it as the `EchoException` message while the
    /// code/domain/operation travel as typed fields.
    pub fn wire_message(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.code, self.domain, self.operation, self.detail
        )
    }
}

/// Seam to the real core: the binding surface calls the core ONLY through
/// this trait. [`CoreSession`] below is the one implementation in this
/// crate; wiring deeper kernel transitions later touches this `impl`, not
/// every call site.
pub trait CoreBackend: Send + Sync {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError>;
    fn echo_counter(&self, decimal: &str, operation: &'static str) -> Result<String, EchoError>;
}

/// Cooperative cancellation flag, session-scoped AbortSignal flavour.
#[derive(Debug, Default)]
pub struct CancelFlag {
    armed: AtomicBool,
}

impl CancelFlag {
    pub fn cancel(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    pub fn take(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }

    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    pub fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

/// Binding-side chunked worker: cooperative cancellation for long calls
/// (foreign callers cancel from another thread).
pub fn echo_bytes_chunked(
    input: &[u8],
    chunks: u32,
    cancel: &CancelFlag,
    operation: &'static str,
) -> Result<Vec<u8>, EchoError> {
    if input.len() as u64 > ubm_core::contracts::MAX_OPERATION_BYTES {
        return Err(EchoError::new(
            "bytes.too-large",
            "core",
            operation,
            "exceeds-max-operation-bytes",
        ));
    }
    if chunks == 0 || chunks > 1_000_000 {
        return Err(EchoError::new(
            "argument.invalid",
            "core",
            operation,
            "chunk-count-range",
        ));
    }
    if cancel.take() {
        return Err(EchoError::new(
            "operation.aborted",
            "core",
            operation,
            "cancelled-before-start",
        ));
    }
    let mut checksum: u64 = 0;
    for _ in 0..chunks {
        if cancel.is_armed() {
            cancel.disarm();
            return Err(EchoError::new(
                "operation.aborted",
                "core",
                operation,
                "cancelled",
            ));
        }
        for &b in input {
            checksum = checksum.wrapping_add(b as u64);
        }
    }
    std::hint::black_box(checksum);
    Ok(input.to_vec())
}

/// Core-backed session. Owns init state, cancellation, and destroyed
/// state; every method fails closed outside its valid lifetime. Contract
/// validation delegates to `ubm-core`.
#[derive(Debug)]
pub struct CoreSession {
    destroyed: bool,
    cancel: Arc<CancelFlag>,
}

impl CoreSession {
    /// Opens a session; rejects a foreign contract revision loudly
    /// (`protocol.incompatible`) instead of operating degraded.
    pub fn open(revision: &str) -> Result<Self, EchoError> {
        match ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision) {
            Ok(()) => Ok(Self {
                destroyed: false,
                cancel: Arc::new(CancelFlag::default()),
            }),
            Err(_) => Err(EchoError::new(
                "protocol.incompatible",
                "core",
                "echo-session.open",
                "contract-revision.mismatch",
            )),
        }
    }

    fn check_usable(&self, operation: &'static str) -> Result<(), EchoError> {
        if self.destroyed {
            return Err(EchoError::new(
                "lifecycle.destroyed",
                "core",
                operation,
                "session-closed",
            ));
        }
        Ok(())
    }

    pub fn cancel_inflight(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_flag(&self) -> Arc<CancelFlag> {
        Arc::clone(&self.cancel)
    }

    pub fn close(&mut self) {
        self.destroyed = true;
        self.cancel.cancel();
    }
}

impl CoreBackend for CoreSession {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
        self.check_usable(operation)?;
        if input.len() as u64 > ubm_core::contracts::MAX_OPERATION_BYTES {
            return Err(EchoError::new(
                "bytes.too-large",
                "core",
                operation,
                "exceeds-max-operation-bytes",
            ));
        }
        Ok(input.to_vec())
    }

    fn echo_counter(&self, decimal: &str, operation: &'static str) -> Result<String, EchoError> {
        self.check_usable(operation)?;
        match ubm_core::contracts::parse_u64_decimal(decimal) {
            Ok(value) => Ok(value.to_string()),
            Err(core) => Err(EchoError::new(
                "bytes.invalid",
                "core",
                operation,
                if core.operation() == "u64.range" {
                    "u64.range"
                } else {
                    "u64.input"
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = CONTRACT_REVISION;

    #[test]
    fn revision_is_the_frozen_contract() {
        assert_eq!(REV, "C-UBM.0.1.1-DRAFT");
        assert_eq!(REV, ubm_core::contracts::CONTRACT_REVISION);
    }

    #[test]
    fn revision_mismatch_rejects_loudly() {
        let err = CoreSession::open("C-UBM.9.9.9-DRAFT").expect_err("must reject");
        assert_eq!((err.code, err.domain), ("protocol.incompatible", "core"));
    }

    #[test]
    fn superseded_revision_rejects_loudly() {
        // The 0.1.0 feasibility revision is no longer spoken: contract truth
        // moved to ubm-core at 0.1.1, and the old pin fails closed.
        let err = CoreSession::open("C-UBM.0.1.0-DRAFT").expect_err("old revision must reject");
        assert_eq!((err.code, err.domain), ("protocol.incompatible", "core"));
    }

    #[test]
    fn byte_batch_round_trip_is_owned() {
        let core = CoreSession::open(REV).unwrap();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = CoreBackend::echo_bytes(&core, &input, "echo-bytes").unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr());
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[], "echo-bytes").unwrap(),
            vec![]
        );
        let err = CoreBackend::echo_bytes(
            &core,
            &vec![0u8; ubm_core::contracts::MAX_OPERATION_BYTES as usize + 1],
            "echo-bytes",
        )
        .expect_err("oversize");
        assert_eq!(err.code, "bytes.too-large");
    }

    #[test]
    fn u64_extremes_lossless_garbage_rejects() {
        let core = CoreSession::open(REV).unwrap();
        for (input, expected) in [
            ("0", "0"),
            ("00042", "42"),
            ("9007199254740993", "9007199254740993"),
            ("18446744073709551615", "18446744073709551615"),
        ] {
            assert_eq!(
                CoreBackend::echo_counter(&core, input, "echo-counter").unwrap(),
                expected
            );
        }
        for bad in ["", "-1", "12a34", "18446744073709551616"] {
            assert_eq!(
                CoreBackend::echo_counter(&core, bad, "echo-counter")
                    .expect_err("bad")
                    .code,
                "bytes.invalid"
            );
        }
    }

    #[test]
    fn u64_range_detail_marks_overflow() {
        let core = CoreSession::open(REV).unwrap();
        let err = CoreBackend::echo_counter(&core, "18446744073709551616", "echo-counter")
            .expect_err("overflow");
        assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.range"));
    }

    #[test]
    fn cancel_armed_aborts_next_chunked_call() {
        let core = CoreSession::open(REV).unwrap();
        core.cancel_inflight();
        let flag = core.cancel_flag();
        let err =
            echo_bytes_chunked(&[1, 2, 3], 10, &flag, "echo-bytes-chunked").expect_err("abort");
        assert_eq!(err.code, "operation.aborted");
        assert!(!flag.is_armed());
    }

    #[test]
    fn close_invalidates_loudly() {
        let mut core = CoreSession::open(REV).unwrap();
        core.close();
        core.close();
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[1], "echo-bytes")
                .expect_err("closed")
                .code,
            "lifecycle.destroyed"
        );
    }

    #[test]
    fn seam_holds_for_the_wired_core() {
        fn assert_backend<T: CoreBackend>(_: &T) {}
        assert_backend(&CoreSession::open(REV).unwrap());
    }

    #[test]
    fn wire_message_names_code_domain_operation() {
        let err = EchoError::new("bytes.invalid", "core", "echo-counter", "u64.input");
        assert_eq!(
            err.wire_message(),
            "bytes.invalid|core|echo-counter|u64.input"
        );
    }
}
