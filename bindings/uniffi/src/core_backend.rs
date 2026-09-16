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
//! The echo transport itself stays feasibility-echo (NOT BLE functionality).
//! U7 transition-driving (U7 slice): [`CoreSession`] additionally holds a
//! REAL [`ubm_core::central::Central`] (owning the one scheduling kernel),
//! constructed at `open`; the driving methods below run real kernel
//! transitions through it. BLE transitions beyond the driven slice reject
//! loudly with contract identities; nothing unimplemented passes silently.
//!
//! Init contract note: the UDL constructor cannot fail, so the revision gate
//! runs on EVERY method — a foreign revision fails closed with
//! `protocol.incompatible` on every call (no effect without valid init).
//! The same fail-closed shape covers the transition core: when central
//! construction cannot establish its invariant (unreachable with the fixed
//! scope labels, but never assumed), every driving call reports
//! `lifecycle.invariant-violation` instead of operating degraded.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use ubm_core::central::{Central, CentralConfig};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, CoreError, Generation,
};
use ubm_core::ownership::EffectBatch;

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

/// Effect-batch capacity for driven kernel transitions (same bound as the
/// sibling bindings; the U7 slice admits no operations, so sweeps and
/// destroy stage nothing yet, but the bound still holds).
const DRIVE_EFFECT_CAP: usize = 64;

/// Construction of the session-owned transition core failed: the binding
/// cannot establish its core invariant.
fn construct_failed(operation: &'static str) -> EchoError {
    EchoError::new(
        "lifecycle.invariant-violation",
        "core",
        operation,
        "central-construct-failed",
    )
}

/// Builds the session-owned transition core: one REAL [`Central`] (owning
/// the one scheduling kernel) bound to this binding's fixed attachment
/// scope with a completed handshake (PKG-02).
fn construct_central(operation: &'static str) -> Result<Central, EchoError> {
    let attachment = AttachmentTuple::new(
        AttachmentId::new("ubm-binding-attachment").map_err(|_| construct_failed(operation))?,
        BackendInstanceId::new("ubm-binding-instance").map_err(|_| construct_failed(operation))?,
        BackendGeneration::new("ubm-binding-generation-0")
            .map_err(|_| construct_failed(operation))?,
        AdapterId::new("ubm-binding-adapter").map_err(|_| construct_failed(operation))?,
        AdapterGeneration::new("ubm-binding-adapter-generation-0")
            .map_err(|_| construct_failed(operation))?,
    );
    let generation = Generation::new("ubm-binding-kernel-generation-0")
        .map_err(|_| construct_failed(operation))?;
    Central::new(attachment, generation, CentralConfig::default())
        .map_err(|_| construct_failed(operation))
}

/// Maps a core rejection to the binding wire form. The frozen contract
/// identity (code + domain) is preserved verbatim; the operation names the
/// binding call under test and the detail names the rejector.
fn central_error(core: CoreError, operation: &'static str) -> EchoError {
    EchoError::new(
        core.code().as_str(),
        core.domain().as_str(),
        operation,
        "central-rejected",
    )
}

/// Parses a host-supplied monotonic millisecond reading over the
/// single-owned decimal-string mapping (DATA-02); anything else is
/// `bytes.invalid`, exactly like the counter path.
pub fn parse_monotonic_ms(decimal: &str, operation: &'static str) -> Result<u64, EchoError> {
    match ubm_core::contracts::parse_u64_decimal(decimal) {
        Ok(value) => Ok(value),
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

/// Core-backed session. The revision gate runs on EVERY call (the UDL
/// constructor cannot fail, so fail-closed init lives at the method level);
/// `close` destroys the session and every later call reports
/// `lifecycle.destroyed`. Contract validation delegates to `ubm-core`. U7
/// transition-driving: the session additionally holds the REAL
/// session-owned [`Central`] (absent only when its construction could not
/// establish the core invariant — every driving call then fails closed).
#[derive(Debug)]
pub struct CoreSession {
    revision_valid: bool,
    destroyed: bool,
    cancel: Arc<CancelFlag>,
    central: Option<Central>,
}

impl CoreSession {
    pub fn open(revision: &str) -> Self {
        let revision_valid =
            ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision)
                .is_ok();
        // Infallible constructor: construction failure is recorded as an
        // absent core, and every driving call fails closed on it (documented
        // above). The error itself is unreachable with the fixed scope
        // labels; dropping it here never silences a reachable path because
        // the absence is observed loudly at every use.
        let central = construct_central("echo-session.open").ok();
        Self {
            revision_valid,
            destroyed: false,
            cancel: Arc::new(CancelFlag::default()),
            central,
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

    fn central_ref(&self, operation: &'static str) -> Result<&Central, EchoError> {
        self.check_usable(operation)?;
        self.central.as_ref().ok_or_else(|| {
            EchoError::new(
                "lifecycle.invariant-violation",
                "core",
                operation,
                "central-missing",
            )
        })
    }

    fn central_mut(&mut self, operation: &'static str) -> Result<&mut Central, EchoError> {
        self.check_usable(operation)?;
        self.central.as_mut().ok_or_else(|| {
            EchoError::new(
                "lifecycle.invariant-violation",
                "core",
                operation,
                "central-missing",
            )
        })
    }

    /// Observes the session-owned transition core: a JSON document with the
    /// frozen revision plus the live kernel counters (`live_operations`,
    /// `retained_cleanup`). Same shape as the sibling bindings.
    pub fn central_status(&self, operation: &'static str) -> Result<String, EchoError> {
        let central = self.central_ref(operation)?;
        Ok(format!(
            "{{\"revision\":\"{}\",\"live_operations\":{},\"retained_cleanup\":{}}}",
            CONTRACT_REVISION,
            central.live_operation_count(),
            central.retained_cleanup_count()
        ))
    }

    /// Drives a REAL kernel transition: an expiry sweep of the
    /// session-owned central at host-supplied monotonic time (decimal
    /// string, DATA-02 mapping). Returns the settled-operation count.
    /// Lifetime gates run before input parsing (uniform post-close
    /// semantics).
    pub fn drive_expire_sweep(
        &mut self,
        now_ms_decimal: &str,
        operation: &'static str,
    ) -> Result<u64, EchoError> {
        let central = self.central_mut(operation)?;
        let now_ms = parse_monotonic_ms(now_ms_decimal, operation)?;
        let mut out = EffectBatch::new(DRIVE_EFFECT_CAP);
        match central.expire_sweep(now_ms, &mut out) {
            Ok((settled, _truncated)) => Ok(settled as u64),
            Err(core) => Err(central_error(core, operation)),
        }
    }

    /// Drives the REAL shutdown transition of the session-owned central and
    /// reports the retained cleanup state (`released` / `release-failed`).
    /// Idempotent.
    pub fn drive_destroy(&mut self, operation: &'static str) -> Result<&'static str, EchoError> {
        let central = self.central_mut(operation)?;
        let mut out = EffectBatch::new(DRIVE_EFFECT_CAP);
        match central.destroy(&mut out) {
            Ok(record) => Ok(match record.state() {
                ubm_core::ownership::CleanupState::Released => "released",
                ubm_core::ownership::CleanupState::ReleaseFailed => "release-failed",
            }),
            Err(core) => Err(central_error(core, operation)),
        }
    }

    /// Loud rejection for BLE transitions beyond the driven slice (scan,
    /// connect, GATT, subscribe, ...): this boundary has no radio/host, so
    /// every named transition fails closed with
    /// `capability.unsupported|capability` (the frozen contract pairing),
    /// never a silent no-op or a faked success. An empty transition name is
    /// `argument.invalid`.
    pub fn request_ble_transition(
        &self,
        transition: &str,
        operation: &'static str,
    ) -> Result<(), EchoError> {
        self.check_usable(operation)?;
        if transition.is_empty() {
            return Err(EchoError::new(
                "argument.invalid",
                "core",
                operation,
                "transition-name-empty",
            ));
        }
        Err(EchoError::new(
            "capability.unsupported",
            "capability",
            operation,
            "transition-not-wired-in-u7-slice",
        ))
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

    /// U7: observes the session-owned REAL Central (frozen revision plus
    /// live kernel counters as JSON).
    pub fn central_status(&self) -> Result<String, EchoError> {
        const OP: &str = "central-status";
        let guard = self.lock(OP)?;
        guard.central_status(OP)
    }

    /// U7: drives a real kernel expiry sweep at decimal-string host time;
    /// returns the settled-operation count as decimal. The revision/close
    /// gate runs before input parsing (uniform post-close semantics); the
    /// inner drive re-checks harmlessly.
    pub fn drive_expire_sweep(&self, now_ms_decimal: &str) -> Result<String, EchoError> {
        const OP: &str = "central-expire-sweep";
        let mut guard = self.lock(OP)?;
        guard.check_usable(OP)?;
        guard
            .drive_expire_sweep(now_ms_decimal, OP)
            .map(|settled| settled.to_string())
    }

    /// U7: drives the real shutdown transition; returns
    /// `released` / `release-failed`. Idempotent.
    pub fn drive_destroy(&self) -> Result<String, EchoError> {
        const OP: &str = "central-destroy";
        let mut guard = self.lock(OP)?;
        guard
            .drive_destroy(OP)
            .map(std::string::ToString::to_string)
    }

    /// U7 loud rejection for BLE transitions beyond the driven slice:
    /// `capability.unsupported|capability`, never silent or faked.
    pub fn request_ble_transition(&self, transition: &str) -> Result<(), EchoError> {
        const OP: &str = "request-ble-transition";
        let guard = self.lock(OP)?;
        guard.request_ble_transition(transition, OP)
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

    #[test]
    fn session_holds_a_real_central() {
        // U7 transition-driving: open constructs a REAL Central (owning the
        // one kernel): zero live operations, zero retained cleanup.
        let core = CoreSession::open(REV).into_shared();
        assert_eq!(
            core.central_status().unwrap(),
            "{\"revision\":\"C-UBM.0.1.1-DRAFT\",\"live_operations\":0,\"retained_cleanup\":0}"
        );
    }

    #[test]
    fn expire_sweep_drives_the_kernel() {
        let core = CoreSession::open(REV).into_shared();
        assert_eq!(core.drive_expire_sweep("0").unwrap(), "0");
        assert_eq!(
            core.drive_expire_sweep("18446744073709551615").unwrap(),
            "0"
        );
        let err = core.drive_expire_sweep("nope").expect_err("garbage time");
        assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.input"));
    }

    #[test]
    fn destroy_drives_shutdown_and_is_idempotent() {
        let core = CoreSession::open(REV).into_shared();
        assert_eq!(core.drive_destroy().unwrap(), "released");
        assert_eq!(core.drive_destroy().unwrap(), "released");
        assert_eq!(core.echo_bytes(&[9]).unwrap(), vec![9]);
    }

    #[test]
    fn unwired_transitions_reject_with_capability_unsupported() {
        let core = CoreSession::open(REV).into_shared();
        for transition in ["scan.start", "queue-advertisement", "force-disconnect"] {
            let err = core
                .request_ble_transition(transition)
                .expect_err("unwired transition must reject");
            assert_eq!(
                (err.code, err.domain, err.operation, err.detail),
                (
                    "capability.unsupported",
                    "capability",
                    "request-ble-transition",
                    "transition-not-wired-in-u7-slice"
                ),
                "transition {transition}"
            );
        }
        let err = core.request_ble_transition("").expect_err("empty name");
        assert_eq!(
            (err.code, err.domain, err.detail),
            ("argument.invalid", "core", "transition-name-empty")
        );
    }

    #[test]
    fn driving_gates_on_revision_and_close() {
        // The revision gate runs on EVERY driving call (foreign revision:
        // protocol.incompatible, no effect). After close every driving call
        // reports lifecycle.destroyed — including garbage sweep input
        // (gate-first ordering, uniform post-close semantics).
        let foreign = CoreSession::open("C-UBM.9.9.9-DRAFT").into_shared();
        for err in [
            foreign.central_status().expect_err("init gate"),
            foreign.drive_expire_sweep("0").expect_err("init gate"),
            foreign.drive_destroy().expect_err("init gate"),
            foreign
                .request_ble_transition("scan.start")
                .expect_err("init gate"),
        ] {
            assert_eq!((err.code, err.domain), ("protocol.incompatible", "core"));
        }
        let core = CoreSession::open(REV).into_shared();
        core.close().unwrap();
        for err in [
            core.central_status().expect_err("closed"),
            core.drive_expire_sweep("0").expect_err("closed"),
            core.drive_expire_sweep("nope").expect_err("closed"),
            core.drive_destroy().expect_err("closed"),
            core.request_ble_transition("scan.start")
                .expect_err("closed"),
        ] {
            assert_eq!((err.code, err.domain), ("lifecycle.destroyed", "core"));
        }
    }

    // NOTE: no wire-join helper lives here on purpose. UniFFI splits the
    // failure into record fields; the Python exchange test reconstructs
    // `code|domain|operation` from LIVE record fields and asserts the
    // exact literal, which is the real proof (a Rust-side join of constants
    // would prove nothing about the boundary).
}
