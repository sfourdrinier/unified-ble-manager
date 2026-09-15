//! Echo-only feasibility core for the UniFFI scaffold (no Tokio, no
//! filesystem, no radio).
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
use std::sync::{Arc, Mutex};

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
/// `cancel` arms the flag; the next unit of session work observes it and
/// reports `operation.aborted`, disarming in the process.
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

/// Binding-side chunked worker: cooperative cancellation over synchronous
/// calls (foreign callers cancel from another thread).
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

/// Feasibility stand-in core. The revision gate runs on EVERY call (the UDL
/// constructor cannot fail, so fail-closed init lives at the method level);
/// `close` destroys the session and every later call reports
/// `lifecycle.destroyed`.
#[derive(Debug)]
pub struct EchoCore {
    revision_valid: bool,
    destroyed: bool,
    cancel: Arc<CancelFlag>,
}

impl EchoCore {
    pub fn open(revision: &str) -> Self {
        Self {
            revision_valid: revision == CONTRACT_REVISION,
            destroyed: false,
            cancel: Arc::new(CancelFlag::default()),
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
        if !self.revision_valid {
            return Err(EchoError::new(
                "protocol.incompatible",
                "core",
                operation,
                "contract-revision.mismatch",
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

    /// Interior-mutability shell for UniFFI objects (shared across threads).
    pub fn into_shared(self) -> SharedCore {
        SharedCore {
            inner: Mutex::new(self),
        }
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

/// Thread-safe shell: the exact object type the UDL interface exposes.
pub struct SharedCore {
    inner: Mutex<EchoCore>,
}

impl SharedCore {
    pub fn echo_bytes(&self, input: &[u8]) -> Result<Vec<u8>, EchoError> {
        let guard = self.lock("echo-bytes")?;
        CoreBackend::echo_bytes(&*guard, input, "echo-bytes")
    }

    pub fn echo_counter(&self, decimal: &str) -> Result<String, EchoError> {
        let guard = self.lock("echo-counter")?;
        CoreBackend::echo_counter(&*guard, decimal, "echo-counter")
    }

    pub fn echo_bytes_chunked(&self, input: &[u8], chunks: u32) -> Result<Vec<u8>, EchoError> {
        const OP: &str = "echo-bytes-chunked";
        let guard = self.lock(OP)?;
        guard.check_usable(OP)?;
        let flag = guard.cancel_flag();
        drop(guard);
        echo_bytes_chunked(input, chunks, &flag, OP)
    }

    pub fn cancel_inflight(&self) -> Result<(), EchoError> {
        let guard = self.lock("cancel-inflight")?;
        guard.check_usable("cancel-inflight")?;
        guard.cancel_inflight();
        Ok(())
    }

    pub fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.close();
        }
    }

    fn lock(
        &self,
        operation: &'static str,
    ) -> Result<std::sync::MutexGuard<'_, EchoCore>, EchoError> {
        self.inner.lock().map_err(|_| {
            EchoError::new(
                "lifecycle.invariant-violation",
                "core",
                operation,
                "lock-poisoned",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = CONTRACT_REVISION;

    #[test]
    fn foreign_revision_fails_closed_on_every_call() {
        let core = EchoCore::open("C-UBM.9.9.9-DRAFT").into_shared();
        for err in [
            core.echo_bytes(&[1]).expect_err("init gate"),
            core.echo_counter("1").expect_err("init gate"),
            core.echo_bytes_chunked(&[1], 1).expect_err("init gate"),
            core.cancel_inflight().expect_err("init gate"),
        ] {
            assert_eq!((err.code, err.domain), ("protocol.incompatible", "core"));
        }
    }

    #[test]
    fn byte_batch_round_trip_is_owned() {
        let core = EchoCore::open(REV).into_shared();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = core.echo_bytes(&input).unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr());
        assert_eq!(core.echo_bytes(&[]).unwrap(), Vec::<u8>::new());
        let err = core
            .echo_bytes(&vec![0u8; MAX_OPERATION_BYTES + 1])
            .expect_err("oversize");
        assert_eq!(err.code, "bytes.too-large");
    }

    #[test]
    fn u64_extremes_lossless_garbage_rejects() {
        let core = EchoCore::open(REV).into_shared();
        for (input, expected) in [
            ("0", "0"),
            ("00042", "42"),
            ("9007199254740993", "9007199254740993"),
            ("18446744073709551615", "18446744073709551615"),
        ] {
            assert_eq!(core.echo_counter(input).unwrap(), expected);
        }
        for bad in ["", "-1", "12a34", "18446744073709551616"] {
            assert_eq!(
                core.echo_counter(bad).expect_err("bad").code,
                "bytes.invalid"
            );
        }
    }

    #[test]
    fn cancel_armed_aborts_next_chunked_call() {
        let core = EchoCore::open(REV).into_shared();
        core.cancel_inflight().unwrap();
        let err = core.echo_bytes_chunked(&[1, 2, 3], 10).expect_err("abort");
        assert_eq!(err.code, "operation.aborted");
        assert_eq!(core.echo_bytes(&[1]).unwrap(), vec![1]);
    }

    #[test]
    fn cancel_mid_flight_aborts_worker_thread() {
        let core = EchoCore::open(REV).into_shared();
        let flag = {
            let guard = core.inner.lock().unwrap();
            guard.cancel_flag()
        };
        let input = vec![0x5Au8; 262_144];
        let worker_flag = Arc::clone(&flag);
        let worker = std::thread::spawn(move || {
            echo_bytes_chunked(&input, 5_000, &worker_flag, "echo-bytes-chunked")
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        flag.cancel();
        let err = worker.join().expect("worker").expect_err("abort");
        assert_eq!(err.code, "operation.aborted");
    }

    #[test]
    fn close_invalidates_loudly_and_idempotently() {
        let core = EchoCore::open(REV).into_shared();
        core.close();
        core.close();
        assert_eq!(
            core.echo_bytes(&[1]).expect_err("closed").code,
            "lifecycle.destroyed"
        );
        assert_eq!(
            core.echo_counter("1").expect_err("closed").code,
            "lifecycle.destroyed"
        );
    }

    #[test]
    fn seam_holds_for_ubm_core_wiring() {
        fn assert_backend<T: CoreBackend>(_: &T) {}
        assert_backend(&EchoCore::open(REV));
    }

    // NOTE: no wire-join helper lives here on purpose. UniFFI splits the
    // failure into record fields; the Python exchange test reconstructs
    // `code|domain|operation|detail` from LIVE record fields and asserts the
    // exact literal, which is the real proof (a Rust-side join of constants
    // would prove nothing about the boundary).
}
