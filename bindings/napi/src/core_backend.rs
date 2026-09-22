//! Core-backed binding core for the N-API binding.
//!
//! The `CoreBackend` seam is implemented for [`CoreSession`], whose contract
//! truth is single-owned by `ubm-core` (frozen `C-UBM.0.1.2-DRAFT`): the
//! revision identity, the byte ceiling, and the decimal-string counter
//! parsing all come from `ubm_core::contracts`. No contract constant or
//! validator is duplicated here — the previous echo-only stand-in
//! (`echo_core.rs`) is deleted, so there are no dual owners.
//!
//! The echo transport itself stays feasibility-echo (owned byte batches back
//! to the caller, NOT BLE functionality). U7 transition-driving (U7 slice):
//! [`CoreSession`] additionally holds and drives a REAL
//! [`ubm_core::central::Central`] (which owns the one scheduling
//! [`ubm_core::ownership::Kernel`]): session open constructs it, and the
//! `central-*` methods below drive real kernel transitions through it.
//! BLE transitions beyond the driven slice reject loudly with contract
//! identities (`capability.unsupported`); nothing unimplemented passes
//! silently.
//!
//! Contract mirror (single-sourced from `ubm-core`, frozen C-UBM.0.1.2-DRAFT):
//! - `CONTRACT_REVISION`: revision mismatch fails closed
//!   (`protocol.incompatible`); no effect before init
//!   (`lifecycle.invalid-state`, mirrors `assertHandshakeComplete`).
//! - `MAX_OPERATION_BYTES = 524288`: oversize is `bytes.too-large`.
//! - `parse_u64_decimal`: u64 values cross as decimal strings; anything else
//!   is `bytes.invalid` (`u64.input` for shape, `u64.range` for overflow).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ubm_core::central::{Central, CentralConfig};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, CoreError, Generation,
};
use ubm_core::ownership::EffectBatch;

/// Frozen contract revision, single-owned by `ubm-core`.
pub use ubm_core::contracts::CONTRACT_REVISION;

/// Synthetic-radio staged driver (U7 staged-transition slice): re-exported
/// for the binding surface. The driver owns a second session-scoped REAL
/// central driven from synthetic host events only; the primary
/// session-owned central above keeps serving status/sweep/destroy
/// untouched.
pub use ubm_fake_radio::{StagedDriver, StagedError};

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

/// Seam to the real core: the binding surface calls the core ONLY through
/// this trait. [`CoreSession`] below is the one implementation in this
/// crate; wiring deeper kernel transitions later touches this `impl`, not
/// every call site.
pub trait CoreBackend: Send + Sync {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError>;
    fn echo_counter(&self, decimal: &str, operation: &'static str) -> Result<String, EchoError>;
}

/// Validate the init revision before any effect (PKG-02 gate), single-sourced
/// from `ubm-core`: the local side is always the frozen revision, so any
/// foreign text fails closed.
pub fn check_revision(revision: &str, operation: &'static str) -> Result<(), EchoError> {
    match ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision) {
        Ok(()) => Ok(()),
        Err(_) => Err(EchoError::new(
            "protocol.incompatible",
            "core",
            operation,
            "contract-revision.mismatch",
        )),
    }
}

/// Owned byte-batch echo against the single-owned ceiling: the input is
/// copied on entry, the output is a fresh allocation. The binding must never
/// retain a borrow of caller memory.
pub fn echo_bytes(input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
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

/// Lossless u64 echo over decimal strings (DATA-02), parsed by
/// `ubm-core`: values above `Number.MAX_SAFE_INTEGER` cross without
/// precision loss. Returns the canonical decimal form (no leading zeros).
/// `ubm-core` reports shape violations as `u64.input` and overflow as
/// `u64.range` in its error operation; both surface here as the detail.
pub fn echo_counter(decimal: &str, operation: &'static str) -> Result<String, EchoError> {
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

/// Core-backed session. Owns init state, cancellation generation, and
/// destroyed state; every method fails closed outside its valid lifetime.
/// Contract validation delegates to `ubm-core`; only the per-session
/// lifecycle lives here.
/// Effect-batch capacity for driven kernel transitions. The U7 slice admits
/// no operations, so sweeps and destroy stage (almost) nothing; the bound
/// still holds and a full batch fails loudly instead of growing.
const DRIVE_EFFECT_CAP: usize = 64;

/// Builds the session-owned transition core: one REAL [`Central`] (owning
/// the one scheduling kernel) bound to this binding's fixed attachment
/// scope with a completed handshake (PKG-02). The scope labels are
/// binding-process constants for this slice (unique per-instance identity
/// is follow-up work); every label is validated, so construction fails
/// loudly instead of operating degraded.
/// Construction of the session-owned transition core failed: the binding
/// cannot establish its core invariant, so the session must not exist.
fn construct_failed(operation: &'static str) -> EchoError {
    EchoError::new(
        "lifecycle.invariant-violation",
        "core",
        operation,
        "central-construct-failed",
    )
}

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

/// Core-backed session. Owns init state, cancellation generation, and
/// destroyed state, plus the REAL session-owned [`Central`] (U7
/// transition-driving): every method fails closed outside its valid
/// lifetime. Contract validation delegates to `ubm-core`; only the
/// per-session lifecycle lives here.
#[derive(Debug)]
pub struct CoreSession {
    initialized: bool,
    destroyed: bool,
    cancel: Arc<CancelFlag>,
    central: Central,
    staged: StagedDriver,
}

impl CoreSession {
    /// Opens a session; rejects a foreign contract revision loudly
    /// (protocol.incompatible) instead of operating degraded. Also
    /// constructs the session-owned transition core, failing loudly when
    /// the core invariant cannot be established.
    pub fn open(revision: &str) -> Result<Self, EchoError> {
        check_revision(revision, "echo-session.open")?;
        Ok(Self {
            initialized: true,
            destroyed: false,
            cancel: Arc::new(CancelFlag::default()),
            central: construct_central("echo-session.open")?,
            staged: StagedDriver::open().map_err(|_| construct_failed("echo-session.open"))?,
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
    /// After `close` this rejects with `lifecycle.destroyed` like every
    /// other call on a destroyed session (uniform with uniffi/JNI).
    pub fn cancel_inflight(&self, operation: &'static str) -> Result<(), EchoError> {
        self.check_usable(operation)?;
        self.cancel.cancel();
        Ok(())
    }

    pub fn cancel_flag(&self) -> Arc<CancelFlag> {
        Arc::clone(&self.cancel)
    }

    /// Observes the session-owned transition core: a JSON document with the
    /// frozen revision plus the live kernel counters (`live_operations`,
    /// `retained_cleanup`). The shape is fixed across bindings; closed
    /// sessions reject with `lifecycle.destroyed` like every other call.
    /// Hand-built JSON (integers plus frozen strings only): no new
    /// dependencies cross this boundary.
    pub fn central_status(&self, operation: &'static str) -> Result<String, EchoError> {
        self.check_usable(operation)?;
        Ok(format!(
            "{{\"revision\":\"{}\",\"live_operations\":{},\"retained_cleanup\":{}}}",
            CONTRACT_REVISION,
            self.central.live_operation_count(),
            self.central.retained_cleanup_count()
        ))
    }

    /// Drives a REAL kernel transition: an expiry sweep of the
    /// session-owned central at host-supplied monotonic time (decimal
    /// string, DATA-02 mapping). Returns the number of operations the kernel
    /// settled by expiry. Lifetime gates run before input parsing, so closed
    /// sessions report `lifecycle.destroyed` even for garbage input
    /// (uniform post-close semantics). The staged effect batch is bounded;
    /// follow-up work surfaces host execution of staged effects (the U7
    /// slice admits no operations, so nothing is staged yet).
    pub fn drive_expire_sweep(
        &mut self,
        now_ms_decimal: &str,
        operation: &'static str,
    ) -> Result<u64, EchoError> {
        self.check_usable(operation)?;
        let now_ms = parse_monotonic_ms(now_ms_decimal, operation)?;
        let mut out = EffectBatch::new(DRIVE_EFFECT_CAP);
        match self.central.expire_sweep(now_ms, &mut out) {
            Ok((settled, _truncated)) => Ok(settled as u64),
            Err(core) => Err(central_error(core, operation)),
        }
    }

    /// Drives the REAL shutdown transition of the session-owned central and
    /// reports the retained cleanup state (`released` when every resource
    /// released cleanly, `release-failed` otherwise). Idempotent: a second
    /// drive reports the retained record. This is the transition core of
    /// `close` without the binding-lifetime side (callback abort, destroyed
    /// flag), which stays on [`CoreSession::close`].
    pub fn drive_destroy(&mut self, operation: &'static str) -> Result<&'static str, EchoError> {
        self.check_usable(operation)?;
        let mut out = EffectBatch::new(DRIVE_EFFECT_CAP);
        match self.central.destroy(&mut out) {
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
    /// `capability.unsupported|capability` (the frozen contract pairing for
    /// unsupported capabilities), never a silent no-op or a faked success.
    /// An empty transition name is `argument.invalid`. Lifecycle gates run
    /// first, so closed sessions still report `lifecycle.destroyed`.
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

    /// Destroys the session. Post-close calls reject with
    /// `lifecycle.destroyed`; close is idempotent.
    pub fn close(&mut self) {
        self.destroyed = true;
        self.cancel.cancel();
    }

    /// Runs one scripted synthetic-radio staged step (a JSON object line)
    /// against the session-owned staged transition core and returns one
    /// JSON observation object. Step-level core rejections come back as
    /// data (`{"ok":false,"error":"code|domain|operation|detail"}`); only
    /// the session lifetime fails closed here (`lifecycle.destroyed` after
    /// `close`, like every call).
    pub fn staged_step(
        &mut self,
        line: &str,
        operation: &'static str,
    ) -> Result<String, StagedError> {
        if self.destroyed {
            return Err(StagedError::new(
                "lifecycle.destroyed",
                "core",
                operation,
                "session-closed",
            ));
        }
        Ok(self.staged.run_step(line))
    }

    /// Drains the staged observation log (FIFO, newline-joined JSON lines).
    pub fn staged_drain(&mut self, operation: &'static str) -> Result<String, StagedError> {
        if self.destroyed {
            return Err(StagedError::new(
                "lifecycle.destroyed",
                "core",
                operation,
                "session-closed",
            ));
        }
        Ok(self.staged.drain_log().join("\n"))
    }

    /// Observes the staged batch accounting as JSON
    /// (`staged_total`, `dropped_not_staged`, `truncated_sweeps`, `cap`).
    /// The `dropped_not_staged` counter is the preserved accounting for
    /// bounded-batch overflow: a full batch fails loudly AND counts here.
    pub fn staged_counters(&self, operation: &'static str) -> Result<String, StagedError> {
        if self.destroyed {
            return Err(StagedError::new(
                "lifecycle.destroyed",
                "core",
                operation,
                "session-closed",
            ));
        }
        Ok(format!(
            "{{\"staged_total\":{},\"dropped_not_staged\":{},\"truncated_sweeps\":{},\"cap\":{}}}",
            self.staged.staged_total(),
            self.staged.dropped_not_staged(),
            self.staged.truncated_sweeps(),
            self.staged.staged_cap()
        ))
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

impl CoreBackend for CoreSession {
    fn echo_bytes(&self, input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
        self.check_usable(operation)?;
        echo_bytes(input, operation)
    }

    fn echo_counter(&self, decimal: &str, operation: &'static str) -> Result<String, EchoError> {
        self.check_usable(operation)?;
        echo_counter(decimal, operation)
    }
}

/// Maximum byte-batch length, single-owned by `ubm-core`.
pub const MAX_OPERATION_BYTES: u64 = ubm_core::contracts::MAX_OPERATION_BYTES;

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = CONTRACT_REVISION;

    #[test]
    fn revision_is_the_frozen_contract() {
        assert_eq!(REV, "C-UBM.0.1.2-DRAFT");
        assert_eq!(REV, ubm_core::contracts::CONTRACT_REVISION);
    }

    #[test]
    fn revision_mismatch_rejects_loudly() {
        let err = CoreSession::open("C-UBM.9.9.9-DRAFT").expect_err("must reject");
        assert_eq!(err.code, "protocol.incompatible");
        assert_eq!(err.domain, "core");
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
        assert_ne!(out.as_ptr(), input.as_ptr(), "must copy, not alias");
    }

    #[test]
    fn empty_batch_round_trips() {
        let core = CoreSession::open(REV).unwrap();
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[], "echo-bytes").unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn oversize_batch_rejects_with_bytes_too_large() {
        let core = CoreSession::open(REV).unwrap();
        let big = vec![7u8; MAX_OPERATION_BYTES as usize + 1];
        let err = CoreBackend::echo_bytes(&core, &big, "echo-bytes").expect_err("must reject");
        assert_eq!(err.code, "bytes.too-large");
        assert_eq!(err.domain, "core");
    }

    #[test]
    fn max_size_batch_round_trips() {
        let core = CoreSession::open(REV).unwrap();
        let big = vec![0xABu8; MAX_OPERATION_BYTES as usize];
        assert_eq!(
            CoreBackend::echo_bytes(&core, &big, "echo-bytes")
                .unwrap()
                .len(),
            MAX_OPERATION_BYTES as usize
        );
    }

    #[test]
    fn u64_extremes_are_lossless() {
        let core = CoreSession::open(REV).unwrap();
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
    fn u64_overflow_rejects_with_range_detail() {
        let core = CoreSession::open(REV).unwrap();
        let err = CoreBackend::echo_counter(&core, "18446744073709551616", "echo-counter")
            .expect_err("u64::MAX+1 must reject");
        assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.range"));
    }

    #[test]
    fn u64_garbage_rejects_with_input_detail() {
        let core = CoreSession::open(REV).unwrap();
        for bad in ["", "-1", "+5", "12a34", " 42", "4.0", "0x10"] {
            let err =
                CoreBackend::echo_counter(&core, bad, "echo-counter").expect_err("must reject");
            assert_eq!(
                (err.code, err.detail, bad),
                ("bytes.invalid", "u64.input", bad)
            );
        }
    }

    #[test]
    fn cancel_before_start_aborts() {
        let core = CoreSession::open(REV).unwrap();
        core.cancel_inflight("cancel-inflight").unwrap();
        let flag = core.cancel_flag();
        let err = echo_bytes_chunked(&[1, 2, 3], 10, &flag, "echo-bytes-async")
            .expect_err("cancelled work must abort");
        assert_eq!(err.code, "operation.aborted");
    }

    #[test]
    fn armed_cancel_aborts_next_dispatch() {
        let core = CoreSession::open(REV).unwrap();
        core.cancel_inflight("cancel-inflight").unwrap();
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
        let core = CoreSession::open(REV).unwrap();
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
    fn cancel_after_close_rejects_destroyed() {
        // M1: `close` invalidates EVERY later call, including cancellation
        // (uniform with uniffi/JNI; the close docstring stands).
        let mut core = CoreSession::open(REV).unwrap();
        core.close();
        let err = core
            .cancel_inflight("cancel-inflight")
            .expect_err("closed cancel must reject");
        assert_eq!(
            (err.code, err.domain, err.operation, err.detail),
            (
                "lifecycle.destroyed",
                "core",
                "cancel-inflight",
                "session-closed"
            )
        );
    }

    #[test]
    fn close_invalidates_loudly() {
        let mut core = CoreSession::open(REV).unwrap();
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
    fn seam_holds_for_the_wired_core() {
        // The binding surface talks to the core ONLY through `CoreBackend`,
        // and the one implementation is the ubm-core-backed session.
        fn assert_backend<T: CoreBackend>(_: &T) {}
        let core = CoreSession::open(REV).unwrap();
        assert_backend(&core);
    }

    #[test]
    fn session_holds_a_real_central() {
        // U7 transition-driving: open constructs a REAL Central (owning the
        // one kernel), observed here with zero live operations and zero
        // retained cleanup on a fresh session.
        let core = CoreSession::open(REV).unwrap();
        assert_eq!(
            core.central_status("central-status").unwrap(),
            "{\"revision\":\"C-UBM.0.1.2-DRAFT\",\"live_operations\":0,\"retained_cleanup\":0}"
        );
    }

    #[test]
    fn expire_sweep_drives_the_kernel() {
        // A fresh central has nothing to expire: the sweep still runs the
        // real kernel transition and settles zero operations, twice, over
        // decimal-string host time (DATA-02 mapping).
        let mut core = CoreSession::open(REV).unwrap();
        assert_eq!(
            core.drive_expire_sweep("0", "central-expire-sweep")
                .unwrap(),
            0
        );
        assert_eq!(
            core.drive_expire_sweep("18446744073709551615", "central-expire-sweep")
                .unwrap(),
            0
        );
        let err = core
            .drive_expire_sweep("nope", "central-expire-sweep")
            .expect_err("garbage time must reject");
        assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.input"));
        assert_eq!(
            core.central_status("central-status").unwrap(),
            "{\"revision\":\"C-UBM.0.1.2-DRAFT\",\"live_operations\":0,\"retained_cleanup\":0}"
        );
    }

    #[test]
    fn staged_surface_drives_synthetic_scan_and_reports_accounting() {
        // The staged drive surface runs synthetic-radio steps through the
        // session-owned staged core and reports bounded-batch accounting.
        // Step-level core rejections arrive as data; only the session
        // lifetime fails closed.
        let mut core = CoreSession::open(REV).unwrap();
        let project = core
            .staged_step("{\"step\":\"cap.project\"}", "staged-step")
            .unwrap();
        assert!(project.contains("\"ok\":true"), "{project}");
        let scan = core
            .staged_step(
                "{\"step\":\"scan.start\",\"op\":\"scan0\",\"owner\":\"owner-a\"}",
                "staged-step",
            )
            .unwrap();
        assert!(scan.contains("\"ok\":true"), "{scan}");
        assert!(scan.contains("central.scan-start"), "{scan}");
        let counters = core.staged_counters("staged-counters").unwrap();
        assert!(counters.contains("\"dropped_not_staged\":0"), "{counters}");
        assert!(counters.contains("\"cap\":64"), "{counters}");
        let loud = core.staged_step("not-json", "staged-step").unwrap();
        assert!(loud.contains("\"ok\":false"), "{loud}");
        assert!(loud.contains("staged-line-not-json"), "{loud}");
        core.close();
        let err = core
            .staged_step("{\"step\":\"cap.project\"}", "staged-step")
            .expect_err("closed session must reject");
        assert_eq!(
            err.wire_message(),
            "lifecycle.destroyed|core|staged-step|session-closed"
        );
    }

    #[test]
    fn monotonic_ms_parses_like_the_counter_path() {
        assert_eq!(parse_monotonic_ms("0", "central-expire-sweep").unwrap(), 0);
        assert_eq!(
            parse_monotonic_ms("00042", "central-expire-sweep").unwrap(),
            42
        );
        for bad in ["", " 42", "+5", "4.0"] {
            let err = parse_monotonic_ms(bad, "central-expire-sweep").expect_err("must reject");
            assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.input"));
        }
        let err = parse_monotonic_ms("18446744073709551616", "central-expire-sweep")
            .expect_err("u64::MAX+1 must reject");
        assert_eq!((err.code, err.detail), ("bytes.invalid", "u64.range"));
    }

    #[test]
    fn destroy_drives_shutdown_and_is_idempotent() {
        // The real shutdown transition releases cleanly on a fresh central
        // and reports the retained record on repeat drives. Driving destroy
        // is orthogonal to the binding lifetime: the session stays usable
        // for echo until `close` (documented split).
        let mut core = CoreSession::open(REV).unwrap();
        assert_eq!(core.drive_destroy("central-destroy").unwrap(), "released");
        assert_eq!(core.drive_destroy("central-destroy").unwrap(), "released");
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[9], "echo-bytes").unwrap(),
            vec![9]
        );
    }

    #[test]
    fn unwired_transitions_reject_with_capability_unsupported() {
        // No fake passes: BLE transitions beyond the driven slice fail
        // closed with the frozen `capability.unsupported|capability`
        // pairing (the actual contract identity for unsupported
        // capabilities), never a silent no-op or faked success.
        let core = CoreSession::open(REV).unwrap();
        for transition in [
            "scan.start",
            "queue-advertisement",
            "force-disconnect",
            "advance-time",
            "emit-notification",
        ] {
            let err = core
                .request_ble_transition(transition, "request-ble-transition")
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
            assert_eq!(
                err.wire_message(),
                "capability.unsupported|capability|request-ble-transition|transition-not-wired-in-u7-slice"
            );
        }
        let err = core
            .request_ble_transition("", "request-ble-transition")
            .expect_err("empty name must reject");
        assert_eq!(
            (err.code, err.domain, err.detail),
            ("argument.invalid", "core", "transition-name-empty")
        );
    }

    #[test]
    fn driving_gates_on_lifetime() {
        // Lifetime/cancel/poison rules are preserved: after `close` every
        // driving call rejects with `lifecycle.destroyed` like every other
        // call (uniform post-close semantics).
        let mut core = CoreSession::open(REV).unwrap();
        core.close();
        for err in [
            core.central_status("central-status").expect_err("closed"),
            core.drive_expire_sweep("0", "central-expire-sweep")
                .expect_err("closed"),
            // Gate-first ordering: even garbage input reports the lifetime,
            // never a parse error (uniform post-close semantics).
            core.drive_expire_sweep("nope", "central-expire-sweep")
                .expect_err("closed"),
            core.drive_destroy("central-destroy").expect_err("closed"),
        ] {
            assert_eq!((err.code, err.domain), ("lifecycle.destroyed", "core"));
        }
        let err = core
            .request_ble_transition("scan.start", "request-ble-transition")
            .expect_err("closed");
        assert_eq!((err.code, err.domain), ("lifecycle.destroyed", "core"));
    }

    #[test]
    fn wire_message_names_code_domain_operation() {
        let err = EchoError::new("bytes.invalid", "core", "echo-counter", "u64.input");
        assert_eq!(
            err.wire_message(),
            "bytes.invalid|core|echo-counter|u64.input"
        );
    }

    #[test]
    fn cleanup_wire_codec_exercises_real_core_records() {
        // The binding links the same core its seam is wired to: drive a real
        // kernel to a retained cleanup record, cross the data-only wire
        // codec, and prove equality. Setup literals cannot fail validation;
        // `expect` here matches the surrounding test style (production paths
        // stay expect-free).
        use ubm_core::contracts::{
            AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
            BackendInstanceId, BleErrorCode, Contender, ContenderKind, Generation, HandshakeState,
            LeaseId, OperationId,
        };
        use ubm_core::ownership::{EffectBatch, Kernel, KernelConfig, KernelInput};

        fn test_attachment() -> AttachmentTuple {
            AttachmentTuple::new(
                AttachmentId::new("attach-01").expect("test id"),
                BackendInstanceId::new("backend-01").expect("test id"),
                BackendGeneration::new("bg-3").expect("test id"),
                AdapterId::new("adapter-01").expect("test id"),
                AdapterGeneration::new("ag-2").expect("test id"),
            )
        }

        let operation = OperationId::new("op-cleanup").expect("test id");
        let mut kernel = Kernel::new(
            KernelConfig::default(),
            test_attachment(),
            Generation::new("gen-1").expect("test id"),
            HandshakeState { complete: true },
        );
        let mut out = EffectBatch::new(16);
        kernel
            .handle(
                KernelInput::Admit {
                    operation_id: operation.clone(),
                    owner: LeaseId::new("lease-1").expect("test id"),
                    attachment: test_attachment(),
                    generation: Generation::new("gen-1").expect("test id"),
                    timeout_ms: 1_000,
                },
                0,
                &mut out,
            )
            .expect("test admit");
        let generation = Generation::new("gen-1").expect("test id");
        kernel
            .handle(
                KernelInput::Dispatch {
                    operation_id: operation.clone(),
                    generation: generation.clone(),
                },
                10,
                &mut out,
            )
            .expect("test dispatch");
        kernel
            .handle(
                KernelInput::Complete {
                    operation_id: operation.clone(),
                    generation: generation.clone(),
                    contender: Contender {
                        ingress_ordinal: 1,
                        kind: ContenderKind::Success,
                        valid: true,
                    },
                },
                20,
                &mut out,
            )
            .expect("test complete");
        kernel
            .handle(
                KernelInput::ReleaseReport {
                    operation_id: operation,
                    ok: false,
                    code: Some(BleErrorCode::ConnectionLost),
                },
                30,
                &mut out,
            )
            .expect("test release report");
        let retained = kernel.drain_cleanup(8);
        assert_eq!(retained.len(), 1, "one retained record expected");
        let record = &retained[0];
        let wire = ubm_core::encode(record);
        assert!(
            wire.contains("\"code\":\"connection.lost\""),
            "wire must carry the data-only failure, got: {wire}"
        );
        let revived = ubm_core::decode(&wire).expect("binding must decode core wire");
        assert!(revived.matches(record));
        assert_eq!(revived.encode(), wire);
    }
}
