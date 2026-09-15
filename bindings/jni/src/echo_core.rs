//! Echo-only feasibility core for the JNI binding (no Tokio, no filesystem,
//! no radio).
//!
//! STAND-IN behind the `CoreBackend` seam: this module proves the boundary
//! exchange (owned byte batches, lossless u64 counters, typed C-UBM error
//! identities, init contract, cooperative cancellation). It is NOT BLE
//! functionality.
//!
//! FOLLOW-UP (explicit): replace `EchoCore` with a real `ubm-core` handle by
//! implementing `CoreBackend` for it once `crates/ubm-core` exists in-tree.
//!
//! Contract mirror (C-UBM.0.1.0-DRAFT, pending U1 acceptance):
//! - `contracts/src/bounds.ts`: `MAX_OPERATION_BYTES = 524288`; u64 values
//!   cross as decimal strings (`parseU64Decimal`); oversize is `bytes.too-large`.
//! - `contracts/src/outcomes.ts`: frozen `code` + `domain` identities below.
//! - `contracts/src/version.ts`: `CONTRACT_REVISION`; revision mismatch fails
//!   closed (`protocol.incompatible`); no effect before init
//!   (`lifecycle.invalid-state`, mirrors `assertHandshakeComplete`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Frozen contract revision this feasibility slice speaks.
pub const CONTRACT_REVISION: &str = "C-UBM.0.1.0-DRAFT";

/// Mirror of `MAX_OPERATION_BYTES` (contracts/src/bounds.ts).
pub const MAX_OPERATION_BYTES: usize = 524288;

/// Largest u64 value, decimal form (DATA-02 lossless-counter mapping).
pub const U64_MAX_DECIMAL: &str = "18446744073709551615";

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

/// Seam for `ubm-core` wiring (explicit follow-up): the real core implements
/// this trait and the binding calls it instead of [`EchoCore`]. The binding
/// surface calls the core ONLY through this trait, so wiring `ubm-core` later
/// touches one `impl`, not every call site.
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
    if input.len() > MAX_OPERATION_BYTES {
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

/// Feasibility stand-in core. Owns init state, cancellation, and destroyed
/// state; every method fails closed outside its valid lifetime.
#[derive(Debug)]
pub struct EchoCore {
    destroyed: bool,
    cancel: Arc<CancelFlag>,
}

impl EchoCore {
    /// Opens a session; rejects a foreign contract revision loudly
    /// (`protocol.incompatible`) instead of operating degraded.
    pub fn open(revision: &str) -> Result<Self, EchoError> {
        if revision != CONTRACT_REVISION {
            return Err(EchoError::new(
                "protocol.incompatible",
                "core",
                "echo-session.open",
                "contract-revision.mismatch",
            ));
        }
        Ok(Self {
            destroyed: false,
            cancel: Arc::new(CancelFlag::default()),
        })
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

impl CoreBackend for EchoCore {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
        self.check_usable(operation)?;
        if input.len() > MAX_OPERATION_BYTES {
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
        if decimal.is_empty()
            || decimal.len() > U64_MAX_DECIMAL.len()
            || !decimal.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(EchoError::new(
                "bytes.invalid",
                "core",
                operation,
                "u64.input",
            ));
        }
        let stripped = decimal.trim_start_matches('0');
        let canonical = if stripped.is_empty() { "0" } else { stripped };
        if canonical.len() > U64_MAX_DECIMAL.len()
            || (canonical.len() == U64_MAX_DECIMAL.len() && canonical > U64_MAX_DECIMAL)
        {
            return Err(EchoError::new(
                "bytes.invalid",
                "core",
                operation,
                "u64.range",
            ));
        }
        Ok(canonical.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = CONTRACT_REVISION;

    #[test]
    fn revision_mismatch_rejects_loudly() {
        let err = EchoCore::open("C-UBM.9.9.9-DRAFT").expect_err("must reject");
        assert_eq!((err.code, err.domain), ("protocol.incompatible", "core"));
    }

    #[test]
    fn byte_batch_round_trip_is_owned() {
        let core = EchoCore::open(REV).unwrap();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = CoreBackend::echo_bytes(&core, &input, "echo-bytes").unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr());
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[], "echo-bytes").unwrap(),
            vec![]
        );
        let err = CoreBackend::echo_bytes(&core, &vec![0u8; MAX_OPERATION_BYTES + 1], "echo-bytes")
            .expect_err("oversize");
        assert_eq!(err.code, "bytes.too-large");
    }

    #[test]
    fn u64_extremes_lossless_garbage_rejects() {
        let core = EchoCore::open(REV).unwrap();
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
    fn cancel_armed_aborts_next_chunked_call() {
        let core = EchoCore::open(REV).unwrap();
        core.cancel_inflight();
        let flag = core.cancel_flag();
        let err =
            echo_bytes_chunked(&[1, 2, 3], 10, &flag, "echo-bytes-chunked").expect_err("abort");
        assert_eq!(err.code, "operation.aborted");
        assert!(!flag.is_armed());
    }

    #[test]
    fn close_invalidates_loudly() {
        let mut core = EchoCore::open(REV).unwrap();
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
    fn seam_holds_for_ubm_core_wiring() {
        fn assert_backend<T: CoreBackend>(_: &T) {}
        assert_backend(&EchoCore::open(REV).unwrap());
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
