//! Core-backed binding core for the JNI binding (no Tokio, no filesystem,
//! no radio).
//!
//! The `CoreBackend` seam is implemented for [`CoreSession`], whose contract
//! truth is single-owned by `ubm-core` (frozen `C-UBM.0.1.2-DRAFT`): the
//! revision identity, the byte ceiling, and the decimal-string counter
//! parsing all come from `ubm_core::contracts`. No contract constant or
//! validator is duplicated here — the previous echo-only stand-in
//! (`echo_core.rs`) is deleted, so there are no dual owners.
//!
//! The echo transport itself stays feasibility-echo (NOT BLE functionality).
//! U7 transition-driving (U7 slice): [`CoreSession`] additionally holds and
//! drives a REAL [`ubm_core::central::Central`] (owning the one scheduling
//! kernel), constructed at `open`; the driving methods below run real kernel
//! transitions through it. BLE transitions beyond the driven slice reject
//! loudly with contract identities; nothing unimplemented passes silently.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::gatt_queue::GattQueue;

use ubm_core::central::{Central, CentralConfig};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, CoreError, Generation,
};
use ubm_core::ownership::EffectBatch;

/// Frozen contract revision, single-owned by `ubm-core`.
pub use ubm_core::contracts::CONTRACT_REVISION;

/// Synthetic-radio staged driver (U7 staged-transition slice): the session
/// holds a second session-scoped REAL central driven from synthetic host
/// events only; the primary central above keeps serving
/// status/sweep/destroy untouched.
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

/// Effect-batch capacity for driven kernel transitions (same bound as the
/// sibling bindings; the U7 slice admits no operations, so sweeps and
/// destroy stage nothing yet, but the bound still holds).
pub(crate) const DRIVE_EFFECT_CAP: usize = 64;

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
pub(crate) fn central_error(core: CoreError, operation: &'static str) -> EchoError {
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

/// Core-backed session. Owns init state, cancellation, and destroyed
/// state, plus the REAL session-owned [`Central`] (U7 transition-driving);
/// every method fails closed outside its valid lifetime. Contract
/// validation delegates to `ubm-core`.
#[derive(Debug)]
pub struct CoreSession {
    destroyed: bool,
    cancel: Arc<CancelFlag>,
    central: Central,
    staged: StagedDriver,
    pub(crate) gatt_queue: GattQueue,
    pub(crate) gatt_resets: u64,
}

impl CoreSession {
    /// Opens a session; rejects a foreign contract revision loudly
    /// (`protocol.incompatible`) instead of operating degraded. Also
    /// constructs the session-owned transition core, failing loudly when
    /// the core invariant cannot be established.
    pub fn open(revision: &str) -> Result<Self, EchoError> {
        match ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision) {
            Ok(()) => Ok(Self {
                destroyed: false,
                cancel: Arc::new(CancelFlag::default()),
                central: construct_central("echo-session.open")?,
                staged: StagedDriver::open().map_err(|_| construct_failed("echo-session.open"))?,
                gatt_queue: VecDeque::new(),
                gatt_resets: 0,
            }),
            Err(_) => Err(EchoError::new(
                "protocol.incompatible",
                "core",
                "echo-session.open",
                "contract-revision.mismatch",
            )),
        }
    }

    pub(crate) fn check_usable(&self, operation: &'static str) -> Result<(), EchoError> {
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

    pub(crate) fn central_mut(&mut self) -> &mut Central {
        &mut self.central
    }

    /// Recycles the per-line staging after a driven GATT line: takes the
    /// staged kernel effects out of the batch (intentionally not surfaced
    /// yet — executor follow-up; the per-line JSON observation already
    /// carries what the host needs) and drains the retained typed-effect
    /// ledger, so both bounded caps recycle across long drains.
    pub(crate) fn count_effects(&mut self, batch: &mut EffectBatch) {
        let _ = batch.drain();
        let _ = self.central.drain_typed_effects();
    }

    pub(crate) fn drained_effects(&mut self, batch: &mut EffectBatch) {
        let _ = batch.drain();
        let _ = self.central.drain_typed_effects();
    }

    pub fn cancel_inflight(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_flag(&self) -> Arc<CancelFlag> {
        Arc::clone(&self.cancel)
    }

    /// Observes the session-owned transition core: a JSON document with the
    /// frozen revision plus the live kernel counters (`live_operations`,
    /// `retained_cleanup`). Same shape as the sibling bindings.
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
    /// string, DATA-02 mapping). Returns the settled-operation count.
    /// Lifetime gates run before input parsing (uniform post-close
    /// semantics).
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
    /// reports the retained cleanup state (`released` / `release-failed`).
    /// Idempotent.
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

    pub fn close(&mut self) {
        self.destroyed = true;
        self.cancel.cancel();
    }

    /// Runs one scripted synthetic-radio staged step (a JSON object line)
    /// against the session-owned staged transition core and returns one
    /// JSON observation object. Step-level core rejections come back as
    /// data (`{"ok":false,...}`); only the session lifetime fails closed
    /// (`lifecycle.destroyed` after `close`, like every call).
    pub fn staged_step(
        &mut self,
        line: &str,
        operation: &'static str,
    ) -> Result<String, StagedError> {
        self.check_usable_staged(operation)?;
        Ok(self.staged.run_step(line))
    }

    /// Drains the staged observation log (FIFO, newline-joined JSON lines).
    pub fn staged_drain(&mut self, operation: &'static str) -> Result<String, StagedError> {
        self.check_usable_staged(operation)?;
        Ok(self.staged.drain_log().join("\n"))
    }

    /// Observes the staged batch accounting as JSON
    /// (`staged_total`, `dropped_not_staged`, `truncated_sweeps`, `cap`).
    pub fn staged_counters(&self, operation: &'static str) -> Result<String, StagedError> {
        self.check_usable_staged(operation)?;
        Ok(format!(
            "{{\"staged_total\":{},\"dropped_not_staged\":{},\"truncated_sweeps\":{},\"cap\":{}}}",
            self.staged.staged_total(),
            self.staged.dropped_not_staged(),
            self.staged.truncated_sweeps(),
            self.staged.staged_cap()
        ))
    }

    fn check_usable_staged(&self, operation: &'static str) -> Result<(), StagedError> {
        if self.destroyed {
            return Err(StagedError::new(
                "lifecycle.destroyed",
                "core",
                operation,
                "session-closed",
            ));
        }
        Ok(())
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
        assert_eq!(REV, "C-UBM.0.1.2-DRAFT");
        assert_eq!(REV, ubm_core::contracts::CONTRACT_REVISION);
    }

    #[test]
    fn staged_surface_drives_synthetic_steps_and_accounting() {
        let mut core = CoreSession::open(REV).unwrap();
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

    #[test]
    fn session_holds_a_real_central() {
        // U7 transition-driving: open constructs a REAL Central (owning the
        // one kernel): zero live operations, zero retained cleanup.
        let core = CoreSession::open(REV).unwrap();
        assert_eq!(
            core.central_status("central-status").unwrap(),
            "{\"revision\":\"C-UBM.0.1.2-DRAFT\",\"live_operations\":0,\"retained_cleanup\":0}"
        );
    }

    #[test]
    fn expire_sweep_drives_the_kernel() {
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
    }

    #[test]
    fn destroy_drives_shutdown_and_is_idempotent() {
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
        let core = CoreSession::open(REV).unwrap();
        for transition in ["scan.start", "queue-advertisement", "force-disconnect"] {
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
        // After `close` every driving call rejects with `lifecycle.destroyed`
        // like every other call — including garbage sweep input (gate-first
        // ordering, uniform post-close semantics).
        let mut core = CoreSession::open(REV).unwrap();
        core.close();
        for err in [
            core.central_status("central-status").expect_err("closed"),
            core.drive_expire_sweep("0", "central-expire-sweep")
                .expect_err("closed"),
            core.drive_expire_sweep("nope", "central-expire-sweep")
                .expect_err("closed"),
            core.drive_destroy("central-destroy").expect_err("closed"),
            core.request_ble_transition("scan.start", "request-ble-transition")
                .expect_err("closed"),
        ] {
            assert_eq!((err.code, err.domain), ("lifecycle.destroyed", "core"));
        }
    }
}
