//! Echo-only feasibility core for the N-API binding.
//!
//! STAND-IN behind the `CoreBackend` seam: this module proves the binding
//! exchange (owned byte batches, lossless u64 counters, typed C-UBM error
//! identities, init contract, cooperative cancellation). It is NOT BLE
//! functionality.
//!
//! FOLLOW-UP (explicit): replace `EchoCore` with a real `ubm-core` handle by
//! implementing `CoreBackend` for it once `crates/ubm-core` exists in-tree.
//! No caller outside this crate may depend on `EchoCore` semantics.
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
    pub fn wire_message(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.code, self.domain, self.operation, self.detail
        )
    }
}

impl std::fmt::Display for EchoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.wire_message())
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

/// Validate the init revision before any effect (PKG-02 gate).
pub fn check_revision(revision: &str, operation: &'static str) -> Result<(), EchoError> {
    if revision == CONTRACT_REVISION {
        Ok(())
    } else {
        Err(EchoError::new(
            "protocol.incompatible",
            "core",
            operation,
            "contract-revision.mismatch",
        ))
    }
}

/// Owned byte-batch echo: the input is copied on entry, the output is a fresh
/// allocation. The binding must never retain a borrow of caller memory.
pub fn echo_bytes(input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
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

/// Lossless u64 echo over decimal strings (DATA-02): values above
/// `Number.MAX_SAFE_INTEGER` cross without precision loss. Returns the
/// canonical decimal form (no leading zeros). Anything else is `bytes.invalid`.
pub fn echo_counter(decimal: &str, operation: &'static str) -> Result<String, EchoError> {
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

/// Cooperative cancellation flag, session-scoped AbortSignal flavour.
/// `cancel` arms the flag; the next unit of session work observes it and
/// reports `operation.aborted`, disarming in the process. Cancellation
/// requests; the result reports what happened. Single in-flight async unit per
/// session is the tested envelope (see LIFETIME_RULES.md).
#[derive(Debug, Default)]
pub struct CancelFlag {
    armed: AtomicBool,
}

impl CancelFlag {
    /// Arms the flag. Idempotent: arming twice still disarms on first abort.
    pub fn cancel(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Atomically observes and disarms. Returns true when a cancel was armed.
    pub fn take(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }

    /// Non-consuming observation for mid-flight chunk checks.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// Disarms without reporting (used when aborting mid-flight).
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

/// Feasibility stand-in core. Owns init state, cancellation generation, and
/// destroyed state; every method fails closed outside its valid lifetime.
#[derive(Debug)]
pub struct EchoCore {
    initialized: bool,
    destroyed: bool,
    cancel: Arc<CancelFlag>,
}

impl EchoCore {
    /// Opens a session; rejects a foreign contract revision loudly
    /// (protocol.incompatible) instead of operating degraded.
    pub fn open(revision: &str) -> Result<Self, EchoError> {
        check_revision(revision, "echo-session.open")?;
        Ok(Self {
            initialized: true,
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
        if !self.initialized {
            return Err(EchoError::new(
                "lifecycle.invalid-state",
                "core",
                operation,
                "handshake-incomplete",
            ));
        }
        Ok(())
    }

    /// Arms session cancellation. In-flight chunked work reports
    /// `operation.aborted` at the next chunk boundary; when no work is in
    /// flight, the next dispatched unit reports `operation.aborted` on entry.
    /// Either way the abort disarms the flag, so the session stays usable.
    pub fn cancel_inflight(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_flag(&self) -> Arc<CancelFlag> {
        Arc::clone(&self.cancel)
    }

    /// Destroys the session. Post-close calls reject with
    /// `lifecycle.destroyed`; close is idempotent.
    pub fn close(&mut self) {
        self.destroyed = true;
        self.cancel.cancel();
    }
}

/// Binding-side async worker: composes the lifecycle pre-check (done by the
/// caller at dispatch) with cooperative cancellation. Returns
/// `operation.aborted` when cancellation lands before completion, even at
/// chunk zero (cancel-before-start). Deterministic: no timing involved.
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

impl CoreBackend for EchoCore {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
        self.check_usable(operation)?;
        echo_bytes(input, operation)
    }

    fn echo_counter(&self, decimal: &str, operation: &'static str) -> Result<String, EchoError> {
        self.check_usable(operation)?;
        echo_counter(decimal, operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = CONTRACT_REVISION;

    #[test]
    fn revision_mismatch_rejects_loudly() {
        let err = EchoCore::open("C-UBM.9.9.9-DRAFT").expect_err("must reject");
        assert_eq!(err.code, "protocol.incompatible");
        assert_eq!(err.domain, "core");
    }

    #[test]
    fn byte_batch_round_trip_is_owned() {
        let core = EchoCore::open(REV).unwrap();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = CoreBackend::echo_bytes(&core, &input, "echo-bytes").unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr(), "must copy, not alias");
    }

    #[test]
    fn empty_batch_round_trips() {
        let core = EchoCore::open(REV).unwrap();
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[], "echo-bytes").unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn oversize_batch_rejects_with_bytes_too_large() {
        let core = EchoCore::open(REV).unwrap();
        let big = vec![7u8; MAX_OPERATION_BYTES + 1];
        let err = CoreBackend::echo_bytes(&core, &big, "echo-bytes").expect_err("must reject");
        assert_eq!(err.code, "bytes.too-large");
        assert_eq!(err.domain, "core");
    }

    #[test]
    fn max_size_batch_round_trips() {
        let core = EchoCore::open(REV).unwrap();
        let big = vec![0xABu8; MAX_OPERATION_BYTES];
        assert_eq!(
            CoreBackend::echo_bytes(&core, &big, "echo-bytes")
                .unwrap()
                .len(),
            MAX_OPERATION_BYTES
        );
    }

    #[test]
    fn u64_extremes_are_lossless() {
        let core = EchoCore::open(REV).unwrap();
        for case in [
            "0",
            "1",
            "9007199254740993",
            "9223372036854775807",
            "18446744073709551615",
            "00042",
        ] {
            let out = CoreBackend::echo_counter(&core, case, "echo-counter").unwrap();
            let expected = case.trim_start_matches('0');
            let expected = if expected.is_empty() { "0" } else { expected };
            assert_eq!(out, expected, "case {case}");
        }
    }

    #[test]
    fn u64_overflow_rejects() {
        let core = EchoCore::open(REV).unwrap();
        let err = CoreBackend::echo_counter(&core, "18446744073709551616", "echo-counter")
            .expect_err("u64::MAX+1 must reject");
        assert_eq!(err.code, "bytes.invalid");
    }

    #[test]
    fn u64_garbage_rejects() {
        let core = EchoCore::open(REV).unwrap();
        for bad in ["", "-1", "+5", "12a34", " 42", "4.0", "0x10"] {
            let err =
                CoreBackend::echo_counter(&core, bad, "echo-counter").expect_err("must reject");
            assert_eq!((err.code, bad), ("bytes.invalid", bad));
        }
    }

    #[test]
    fn cancel_before_start_aborts() {
        let core = EchoCore::open(REV).unwrap();
        core.cancel_inflight();
        let flag = core.cancel_flag();
        let err = echo_bytes_chunked(&[1, 2, 3], 10, &flag, "echo-bytes-async")
            .expect_err("cancelled work must abort");
        assert_eq!(err.code, "operation.aborted");
    }

    #[test]
    fn armed_cancel_aborts_next_dispatch() {
        let core = EchoCore::open(REV).unwrap();
        core.cancel_inflight();
        let flag = core.cancel_flag();
        assert!(flag.is_armed());
        let err = echo_bytes_chunked(&[9u8; 64], 1_000, &flag, "echo-bytes-async")
            .expect_err("must abort");
        assert_eq!(err.code, "operation.aborted");
        // The abort disarms the flag: the session stays usable.
        assert!(!flag.is_armed());
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[9], "echo-bytes").unwrap(),
            vec![9]
        );
    }

    #[test]
    fn cancel_mid_flight_aborts_worker_thread() {
        let core = EchoCore::open(REV).unwrap();
        let flag = core.cancel_flag();
        // ~1 GiB of hashing: the worker is guaranteed in-flight when the main
        // thread cancels 20 ms later (25x+ margin, strictly asserted abort).
        let input = vec![0x5Au8; 262_144];
        let worker_flag = Arc::clone(&flag);
        let worker = std::thread::spawn(move || {
            echo_bytes_chunked(&input, 5_000, &worker_flag, "echo-bytes-async")
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        flag.cancel();
        let err = worker
            .join()
            .expect("worker thread")
            .expect_err("must abort");
        assert_eq!((err.code, err.domain), ("operation.aborted", "core"));
        assert!(!flag.is_armed(), "abort disarms the flag");
    }

    #[test]
    fn close_invalidates_loudly() {
        let mut core = EchoCore::open(REV).unwrap();
        core.close();
        // Async dispatch goes through the same seam pre-check at queue time
        // (proven by the JS exchange tests); the worker itself is covered above.
        for err in [
            CoreBackend::echo_bytes(&core, &[1], "echo-bytes").expect_err("closed"),
            CoreBackend::echo_counter(&core, "1", "echo-counter").expect_err("closed"),
        ] {
            assert_eq!(err.code, "lifecycle.destroyed");
        }
        core.close(); // idempotent, no panic
    }

    #[test]
    fn seam_holds_for_ubm_core_wiring() {
        // Compile-time proof that the feasibility core satisfies the seam the
        // real ubm-core handle must implement (explicit follow-up).
        fn assert_backend<T: CoreBackend>(_: &T) {}
        let core = EchoCore::open(REV).unwrap();
        assert_backend(&core);
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
