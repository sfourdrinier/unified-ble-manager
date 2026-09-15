//! Core-backed binding core for the UniFFI scaffold (no Tokio, no
//! filesystem, no radio).
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
//!
//! Init contract note: the UDL constructor cannot fail, so the revision gate
//! runs on EVERY method — a foreign revision fails closed with
//! `protocol.incompatible` on every call (no effect without valid init).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

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

/// Core-backed session. The revision gate runs on EVERY call (the UDL
/// constructor cannot fail, so fail-closed init lives at the method level);
/// `close` destroys the session and every later call reports
/// `lifecycle.destroyed`. Contract validation delegates to `ubm-core`.
#[derive(Debug)]
pub struct CoreSession {
    revision_valid: bool,
    destroyed: bool,
    cancel: Arc<CancelFlag>,
}

impl CoreSession {
    pub fn open(revision: &str) -> Self {
        let revision_valid =
            ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision)
                .is_ok();
        Self {
            revision_valid,
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

/// Thread-safe shell: the exact object type the UDL interface exposes.
pub struct SharedCore {
    inner: Mutex<CoreSession>,
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

    /// Destroys the session. A poisoned lock maps to a loud
    /// `lifecycle.invariant-violation` like every other lock path (L4:
    /// napi reports poison loudly; swallowing it here would skip the
    /// destroy silently).
    pub fn close(&self) -> Result<(), EchoError> {
        let mut guard = self.lock("close")?;
        guard.close();
        Ok(())
    }

    fn lock(
        &self,
        operation: &'static str,
    ) -> Result<std::sync::MutexGuard<'_, CoreSession>, EchoError> {
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
    fn revision_is_the_frozen_contract() {
        assert_eq!(REV, "C-UBM.0.1.1-DRAFT");
        assert_eq!(REV, ubm_core::contracts::CONTRACT_REVISION);
    }

    #[test]
    fn superseded_revision_fails_closed_on_every_call() {
        // The 0.1.0 feasibility revision is no longer spoken: contract truth
        // moved to ubm-core at 0.1.1, and the old pin fails closed.
        let core = CoreSession::open("C-UBM.0.1.0-DRAFT").into_shared();
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
    fn foreign_revision_fails_closed_on_every_call() {
        let core = CoreSession::open("C-UBM.9.9.9-DRAFT").into_shared();
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
        let core = CoreSession::open(REV).into_shared();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = core.echo_bytes(&input).unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr());
        assert_eq!(core.echo_bytes(&[]).unwrap(), Vec::<u8>::new());
        let err = core
            .echo_bytes(&vec![
                0u8;
                ubm_core::contracts::MAX_OPERATION_BYTES as usize + 1
            ])
            .expect_err("oversize");
        assert_eq!(err.code, "bytes.too-large");
    }

    #[test]
    fn u64_extremes_lossless_garbage_rejects() {
        let core = CoreSession::open(REV).into_shared();
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
    fn u64_range_detail_marks_overflow() {
        let core = CoreSession::open(REV).into_shared();
        let err = core
            .echo_counter("18446744073709551616")
            .expect_err("overflow");
        assert_eq!(err.detail, "u64.range");
    }

    #[test]
    fn cancel_armed_aborts_next_chunked_call() {
        let core = CoreSession::open(REV).into_shared();
        core.cancel_inflight().unwrap();
        let err = core.echo_bytes_chunked(&[1, 2, 3], 10).expect_err("abort");
        assert_eq!(err.code, "operation.aborted");
        assert_eq!(core.echo_bytes(&[1]).unwrap(), vec![1]);
    }

    #[test]
    fn cancel_mid_flight_aborts_worker_thread() {
        let core = CoreSession::open(REV).into_shared();
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
        let core = CoreSession::open(REV).into_shared();
        core.close().unwrap();
        core.close().unwrap();
        assert_eq!(
            core.echo_bytes(&[1]).expect_err("closed").code,
            "lifecycle.destroyed"
        );
        assert_eq!(
            core.echo_counter("1").expect_err("closed").code,
            "lifecycle.destroyed"
        );
        assert_eq!(
            core.cancel_inflight().expect_err("closed").code,
            "lifecycle.destroyed"
        );
    }

    #[test]
    fn close_maps_poison_loudly() {
        // L4: a poisoned lock must fail the destroy loudly (napi reports
        // poison loudly); silently skipping the destroy is not an option.
        let core = CoreSession::open(REV).into_shared();
        let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = core.inner.lock().unwrap();
            panic!("poison-injection");
        }));
        assert!(injected.is_err(), "injection must poison the lock");
        let err = core.close().expect_err("poisoned close must be loud");
        assert_eq!(
            (err.code, err.domain, err.operation, err.detail),
            (
                "lifecycle.invariant-violation",
                "core",
                "close",
                "lock-poisoned"
            )
        );
    }

    #[test]
    fn seam_holds_for_the_wired_core() {
        fn assert_backend<T: CoreBackend>(_: &T) {}
        assert_backend(&CoreSession::open(REV));
    }

    // NOTE: no wire-join helper lives here on purpose. UniFFI splits the
    // failure into record fields; the Python exchange test reconstructs
    // `code|domain|operation` from LIVE record fields and asserts the
    // exact literal, which is the real proof (a Rust-side join of constants
    // would prove nothing about the boundary).
}
