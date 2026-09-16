//! Core-backed binding core for the WASM binding (portable, no threads,
//! no Tokio, no filesystem, no radio).
//!
//! The `CoreBackend` seam is implemented for [`CoreSession`], whose contract
//! truth is single-owned by `ubm-core` (frozen `C-UBM.0.1.1-DRAFT`): the
//! revision identity, the byte ceiling, and the decimal-string counter
//! parsing all come from `ubm_core::contracts`. No contract constant or
//! validator is duplicated here — the previous echo-only stand-in
//! (`echo_core.rs`) is deleted, so there are no dual owners.
//!
//! The streaming-echo table stays here: it is per-boundary cooperative
//! plumbing (single-threaded by construction), not contract truth. U7
//! transition-driving (U7 slice): [`CoreSession`] additionally holds a REAL
//! [`ubm_core::central::Central`] (owning the one scheduling kernel),
//! constructed at `init`; the `central-*` methods below drive real kernel
//! transitions through it. BLE transitions beyond the driven slice reject
//! loudly with contract identities; nothing unimplemented passes silently.

/// Frozen contract revision, single-owned by `ubm-core`.
pub use ubm_core::contracts::CONTRACT_REVISION;

use ubm_core::central::{Central, CentralConfig};
use ubm_core::contracts::{
    AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
    BackendInstanceId, BleErrorCode, CoreError, Generation,
};
use ubm_core::ownership::EffectBatch;

/// Mirror of the single-owned `MAX_OPERATION_BYTES`, adapted to `usize` for
/// indexing. The value lives in `ubm-core`; this is a type adaptation, not a
/// second pin.
pub const MAX_OPERATION_BYTES: usize = ubm_core::contracts::MAX_OPERATION_BYTES as usize;

/// Largest u64 wire value as decimal text (DATA-02 mapping), rendered from
/// the single-owned `ubm-core` constant — no duplicated literal.
pub fn u64_max_decimal() -> String {
    ubm_core::contracts::U64_MAX.to_string()
}

/// Numeric error codes for the raw integer ABI. The full typed identity
/// (`code|domain|operation|detail`) is always available alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum EchoCode {
    Ok = 0,
    ArgumentInvalid = 1,
    BytesInvalid = 2,
    BytesTooLarge = 3,
    OperationAborted = 4,
    InvalidState = 5,
    ProtocolIncompatible = 6,
    /// U7 addition (appended: existing values are unchanged): the boundary
    /// has no radio/host, so BLE transitions beyond the driven slice report
    /// the frozen `capability.unsupported` identity with this code.
    CapabilityUnsupported = 7,
}

/// Typed failure carrying a frozen C-UBM `code` + `domain` plus the operation
/// under test. Never silent: every rejection names its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoError {
    pub code: EchoCode,
    pub name: &'static str,
    pub domain: &'static str,
    pub operation: &'static str,
    pub detail: &'static str,
}

impl EchoError {
    pub const fn new(
        code: EchoCode,
        name: &'static str,
        domain: &'static str,
        operation: &'static str,
        detail: &'static str,
    ) -> Self {
        Self {
            code,
            name,
            domain,
            operation,
            detail,
        }
    }

    /// Wire form shared by every binding: `code|domain|operation|detail`.
    pub fn wire_message(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.name, self.domain, self.operation, self.detail
        )
    }

    pub fn argument_invalid(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::ArgumentInvalid,
            "argument.invalid",
            "core",
            operation,
            detail,
        )
    }

    pub fn bytes_invalid(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::BytesInvalid,
            "bytes.invalid",
            "core",
            operation,
            detail,
        )
    }

    pub fn bytes_too_large(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::BytesTooLarge,
            "bytes.too-large",
            "core",
            operation,
            detail,
        )
    }

    pub fn aborted(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::OperationAborted,
            "operation.aborted",
            "core",
            operation,
            detail,
        )
    }

    pub fn invalid_state(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::InvalidState,
            "lifecycle.invalid-state",
            "core",
            operation,
            detail,
        )
    }

    pub fn incompatible(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::ProtocolIncompatible,
            "protocol.incompatible",
            "core",
            operation,
            detail,
        )
    }

    /// U7 loud-rejection identity for BLE transitions beyond the driven
    /// slice. The `capability.unsupported|capability` pairing is the frozen
    /// contract pairing for unsupported capabilities (never domain `core`).
    pub fn capability_unsupported(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::CapabilityUnsupported,
            "capability.unsupported",
            "capability",
            operation,
            detail,
        )
    }

    pub fn invariant_violation(operation: &'static str, detail: &'static str) -> Self {
        Self::new(
            EchoCode::InvalidState,
            "lifecycle.invariant-violation",
            "core",
            operation,
            detail,
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
/// from `ubm-core`.
pub fn check_revision(revision: &str, operation: &'static str) -> Result<(), EchoError> {
    match ubm_core::contracts::assert_contract_revision_equal(CONTRACT_REVISION, revision) {
        Ok(()) => Ok(()),
        Err(_) => Err(EchoError::incompatible(
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
        return Err(EchoError::bytes_too_large(
            operation,
            "exceeds-max-operation-bytes",
        ));
    }
    Ok(input.to_vec())
}

/// Lossless u64 echo over decimal strings (DATA-02), parsed by `ubm-core`.
/// `ubm-core` reports shape violations as `u64.input` and overflow as
/// `u64.range` in its error operation; both surface here as the detail.
pub fn echo_counter(decimal: &str, operation: &'static str) -> Result<String, EchoError> {
    match ubm_core::contracts::parse_u64_decimal(decimal) {
        Ok(value) => Ok(value.to_string()),
        Err(core) => Err(EchoError::bytes_invalid(
            operation,
            if core.operation() == "u64.range" {
                "u64.range"
            } else {
                "u64.input"
            },
        )),
    }
}

/// Effect-batch capacity for driven kernel transitions (same bound as the
/// sibling bindings; the U7 slice admits no operations, so sweeps and
/// destroy stage nothing yet, but the bound still holds).
const DRIVE_EFFECT_CAP: usize = 64;

/// Construction of the session-owned transition core failed: the binding
/// cannot establish its core invariant.
fn construct_failed(operation: &'static str) -> EchoError {
    EchoError::invariant_violation(operation, "central-construct-failed")
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
/// identity (code + domain) travels in the name/domain slots verbatim; the
/// numeric code is the coarse ABI mapping and the operation names the
/// binding call under test.
fn central_error(core: CoreError, operation: &'static str) -> EchoError {
    let code = match core.code() {
        BleErrorCode::ArgumentInvalid => EchoCode::ArgumentInvalid,
        BleErrorCode::BytesInvalid => EchoCode::BytesInvalid,
        BleErrorCode::BytesTooLarge => EchoCode::BytesTooLarge,
        BleErrorCode::OperationAborted => EchoCode::OperationAborted,
        BleErrorCode::ProtocolIncompatible => EchoCode::ProtocolIncompatible,
        BleErrorCode::CapabilityUnsupported => EchoCode::CapabilityUnsupported,
        _ => EchoCode::InvalidState,
    };
    EchoError::new(
        code,
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
        Err(core) => Err(EchoError::bytes_invalid(
            operation,
            if core.operation() == "u64.range" {
                "u64.range"
            } else {
                "u64.input"
            },
        )),
    }
}

/// Core-backed session: init state plus a streaming-echo table for
/// cooperative cancellation, plus the REAL session-owned [`Central`] (U7
/// transition-driving, constructed at `init`). Every method fails closed
/// outside its valid lifetime. Single-threaded by construction (WASM has no
/// threads here).
#[derive(Debug, Default)]
pub struct CoreSession {
    initialized_revision: Option<&'static str>,
    next_stream: u64,
    streams: std::collections::HashMap<u64, StreamState>,
    central: Option<Central>,
}

#[derive(Debug)]
struct StreamState {
    data: Vec<u8>,
    cancelled: bool,
}

impl CoreSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialises the binding. A foreign revision fails closed with
    /// `protocol.incompatible`; re-init with a DIFFERENT revision after a
    /// successful init also fails (binding/core identity must agree, PKG-02).
    /// Re-init with the same revision is a no-op success. Success also
    /// constructs the session-owned transition core (U7): the binding holds
    /// a REAL Central from here on.
    pub fn init(&mut self, revision: &str) -> Result<(), EchoError> {
        check_revision(revision, "echo-init")?;
        if let Some(current) = self.initialized_revision {
            if current != CONTRACT_REVISION {
                return Err(EchoError::incompatible("echo-init", "identity-changed"));
            }
            return Ok(());
        }
        self.central = Some(construct_central("echo-init")?);
        self.initialized_revision = Some(CONTRACT_REVISION);
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized_revision.is_some()
    }

    fn check_usable(&self, operation: &'static str) -> Result<(), EchoError> {
        if !self.is_initialized() {
            return Err(EchoError::invalid_state(operation, "handshake-incomplete"));
        }
        Ok(())
    }

    /// Opens a streaming echo. Returns 0 (never a valid handle) when the
    /// binding is uninitialised, recording `lifecycle.invalid-state`.
    pub fn stream_begin(&mut self, operation: &'static str) -> Result<u64, EchoError> {
        self.check_usable(operation)?;
        self.next_stream = self.next_stream.wrapping_add(1).max(1);
        let handle = self.next_stream;
        self.streams.insert(
            handle,
            StreamState {
                data: Vec::new(),
                cancelled: false,
            },
        );
        Ok(handle)
    }

    fn stream_mut(
        &mut self,
        handle: u64,
        operation: &'static str,
    ) -> Result<&mut StreamState, EchoError> {
        self.check_usable(operation)?;
        self.streams.get_mut(&handle).ok_or_else(|| {
            if handle == 0 {
                EchoError::argument_invalid(operation, "null-handle")
            } else {
                EchoError::invalid_state(operation, "stream-unknown-or-consumed")
            }
        })
    }

    /// Pushes a chunk. Rejects `bytes.too-large` when the accumulated total
    /// would exceed the cap (the stream stays usable); rejects
    /// `operation.aborted` once cancelled (sticky).
    pub fn stream_push(
        &mut self,
        handle: u64,
        chunk: &[u8],
        operation: &'static str,
    ) -> Result<usize, EchoError> {
        let stream = self.stream_mut(handle, operation)?;
        if stream.cancelled {
            return Err(EchoError::aborted(operation, "stream-cancelled"));
        }
        if stream.data.len() as u64 + chunk.len() as u64 > ubm_core::contracts::MAX_OPERATION_BYTES
        {
            return Err(EchoError::bytes_too_large(
                operation,
                "exceeds-max-operation-bytes",
            ));
        }
        stream.data.extend_from_slice(chunk);
        Ok(stream.data.len())
    }

    /// Cancels a stream. Finishing a cancelled stream reports
    /// `operation.aborted` exactly once and consumes the handle.
    pub fn stream_cancel(&mut self, handle: u64, operation: &'static str) -> Result<(), EchoError> {
        let stream = self.stream_mut(handle, operation)?;
        stream.cancelled = true;
        Ok(())
    }

    /// Finishes a stream, consuming the handle. A second finish (or any use
    /// of the consumed handle) rejects loudly — never a silent replay.
    pub fn stream_finish(
        &mut self,
        handle: u64,
        operation: &'static str,
    ) -> Result<Vec<u8>, EchoError> {
        self.check_usable(operation)?;
        let stream = self.streams.remove(&handle).ok_or_else(|| {
            if handle == 0 {
                EchoError::argument_invalid(operation, "null-handle")
            } else {
                EchoError::invalid_state(operation, "stream-unknown-or-consumed")
            }
        })?;
        if stream.cancelled {
            return Err(EchoError::aborted(operation, "stream-cancelled"));
        }
        Ok(stream.data)
    }

    fn central_ref(&self, operation: &'static str) -> Result<&Central, EchoError> {
        self.check_usable(operation)?;
        self.central
            .as_ref()
            .ok_or_else(|| EchoError::invariant_violation(operation, "central-missing"))
    }

    fn central_mut(&mut self, operation: &'static str) -> Result<&mut Central, EchoError> {
        self.check_usable(operation)?;
        self.central
            .as_mut()
            .ok_or_else(|| EchoError::invariant_violation(operation, "central-missing"))
    }

    /// Observes the session-owned transition core: a JSON document with the
    /// frozen revision plus the live kernel counters (`live_operations`,
    /// `retained_cleanup`). Same shape as the sibling bindings. Fails
    /// closed before init like every other call.
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
    /// string, DATA-02 mapping). Returns the number of operations the kernel
    /// settled by expiry. Lifetime gates run before input parsing, so
    /// uninitialised sessions report `lifecycle.invalid-state` even for
    /// garbage input.
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
    /// reports the retained cleanup state (`released` when every resource
    /// released cleanly, `release-failed` otherwise). Idempotent.
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
            return Err(EchoError::argument_invalid(
                operation,
                "transition-name-empty",
            ));
        }
        Err(EchoError::capability_unsupported(
            operation,
            "transition-not-wired-in-u7-slice",
        ))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const OP: &str = "test-op";

    fn initialized() -> CoreSession {
        let mut core = CoreSession::new();
        core.init(CONTRACT_REVISION).unwrap();
        core
    }

    #[test]
    fn revision_is_the_frozen_contract() {
        assert_eq!(CONTRACT_REVISION, "C-UBM.0.1.1-DRAFT");
        assert_eq!(CONTRACT_REVISION, ubm_core::contracts::CONTRACT_REVISION);
        assert_eq!(u64_max_decimal(), "18446744073709551615");
    }

    #[test]
    fn foreign_revision_rejects_closed() {
        let mut core = CoreSession::new();
        let err = core.init("C-UBM.9.9.9-DRAFT").expect_err("must reject");
        assert_eq!(
            (err.code, err.name),
            (EchoCode::ProtocolIncompatible, "protocol.incompatible")
        );
        assert!(!core.is_initialized());
        // Operations before init fail closed, never silently succeed.
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[1], OP)
                .expect_err("uninit")
                .code,
            EchoCode::InvalidState
        );
    }

    #[test]
    fn superseded_revision_rejects_closed() {
        // The 0.1.0 feasibility revision is no longer spoken: contract truth
        // moved to ubm-core at 0.1.1, and the old pin fails closed.
        let mut core = CoreSession::new();
        let err = core
            .init("C-UBM.0.1.0-DRAFT")
            .expect_err("old revision must reject");
        assert_eq!(err.code, EchoCode::ProtocolIncompatible);
        assert!(!core.is_initialized());
    }

    #[test]
    fn reinit_with_different_revision_rejects() {
        let mut core = initialized();
        assert!(core.init(CONTRACT_REVISION).is_ok());
        let err = core.init("C-UBM.0.2.0-DRAFT").expect_err("identity change");
        assert_eq!(err.code, EchoCode::ProtocolIncompatible);
        assert!(core.is_initialized(), "failed re-init must not de-init");
    }

    #[test]
    fn byte_batch_round_trip_is_owned() {
        let core = initialized();
        let input = vec![0u8, 1, 2, 250, 255];
        let out = CoreBackend::echo_bytes(&core, &input, OP).unwrap();
        assert_eq!(out, input);
        assert_ne!(out.as_ptr(), input.as_ptr());
        assert_eq!(
            CoreBackend::echo_bytes(&core, &[], OP).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn oversize_rejects_max_ok() {
        let core = initialized();
        let err = CoreBackend::echo_bytes(&core, &vec![7u8; MAX_OPERATION_BYTES + 1], OP)
            .expect_err("must reject");
        assert_eq!(
            (err.code, err.name),
            (EchoCode::BytesTooLarge, "bytes.too-large")
        );
        let out = CoreBackend::echo_bytes(&core, &vec![7u8; MAX_OPERATION_BYTES], OP).unwrap();
        assert_eq!(out.len(), MAX_OPERATION_BYTES);
    }

    #[test]
    fn u64_extremes_lossless_garbage_rejects() {
        let core = initialized();
        for (input, expected) in [
            ("0", "0"),
            ("1", "1"),
            ("00042", "42"),
            ("9007199254740993", "9007199254740993"),
            ("9223372036854775807", "9223372036854775807"),
            ("18446744073709551615", "18446744073709551615"),
        ] {
            assert_eq!(
                CoreBackend::echo_counter(&core, input, OP).unwrap(),
                expected,
                "case {input}"
            );
        }
        for bad in [
            "",
            "-1",
            "+5",
            "12a34",
            " 42",
            "4.0",
            "0x10",
            "18446744073709551616",
        ] {
            let err = CoreBackend::echo_counter(&core, bad, OP).expect_err("must reject");
            assert_eq!((err.code, bad), (EchoCode::BytesInvalid, bad));
        }
    }

    #[test]
    fn u64_range_detail_marks_overflow() {
        let core = initialized();
        let err =
            CoreBackend::echo_counter(&core, "18446744073709551616", OP).expect_err("overflow");
        assert_eq!(err.detail, "u64.range");
        let err = CoreBackend::echo_counter(&core, "12a34", OP).expect_err("shape");
        assert_eq!(err.detail, "u64.input");
    }

    #[test]
    fn stream_round_trip_then_consumed() {
        let mut core = initialized();
        let h = core.stream_begin(OP).unwrap();
        assert_ne!(h, 0);
        assert_eq!(core.stream_push(h, &[1, 2], OP).unwrap(), 2);
        assert_eq!(core.stream_push(h, &[], OP).unwrap(), 2);
        assert_eq!(core.stream_push(h, &[3], OP).unwrap(), 3);
        assert_eq!(core.stream_finish(h, OP).unwrap(), vec![1, 2, 3]);
        let err = core.stream_finish(h, OP).expect_err("consumed handle");
        assert_eq!(err.code, EchoCode::InvalidState);
        let err = core.stream_push(h, &[9], OP).expect_err("consumed handle");
        assert_eq!(err.code, EchoCode::InvalidState);
    }

    #[test]
    fn stream_cancel_reports_aborted_exactly_once() {
        let mut core = initialized();
        let h = core.stream_begin(OP).unwrap();
        core.stream_push(h, &[5, 6], OP).unwrap();
        core.stream_cancel(h, OP).unwrap();
        let err = core.stream_push(h, &[7], OP).expect_err("cancelled push");
        assert_eq!(err.code, EchoCode::OperationAborted);
        let err = core.stream_finish(h, OP).expect_err("cancelled finish");
        assert_eq!(err.code, EchoCode::OperationAborted);
        // Consumed by the aborted finish: further use is invalid-state, loud.
        let err = core.stream_finish(h, OP).expect_err("consumed");
        assert_eq!(err.code, EchoCode::InvalidState);
    }

    #[test]
    fn stream_guards_reject_loudly() {
        let mut core = CoreSession::new();
        assert_eq!(
            core.stream_begin(OP).expect_err("uninit").code,
            EchoCode::InvalidState
        );
        let mut core = initialized();
        assert_eq!(
            core.stream_push(0, &[1], OP).expect_err("null handle").code,
            EchoCode::ArgumentInvalid
        );
        assert_eq!(
            core.stream_finish(999_999, OP).expect_err("unknown").code,
            EchoCode::InvalidState
        );
        assert_eq!(
            core.stream_cancel(999_999, OP).expect_err("unknown").code,
            EchoCode::InvalidState
        );
        // Oversize push rejects but keeps the stream usable.
        let h = core.stream_begin(OP).unwrap();
        let err = core
            .stream_push(h, &vec![0u8; MAX_OPERATION_BYTES + 1], OP)
            .expect_err("too large");
        assert_eq!(err.code, EchoCode::BytesTooLarge);
        assert_eq!(core.stream_push(h, &[8], OP).unwrap(), 1);
        assert_eq!(core.stream_finish(h, OP).unwrap(), vec![8]);
    }

    #[test]
    fn streams_are_independent() {
        let mut core = initialized();
        let a = core.stream_begin(OP).unwrap();
        let b = core.stream_begin(OP).unwrap();
        assert_ne!(a, b);
        core.stream_push(a, &[1], OP).unwrap();
        core.stream_push(b, &[2, 3], OP).unwrap();
        core.stream_cancel(a, OP).unwrap();
        assert!(core.stream_finish(a, OP).is_err());
        assert_eq!(core.stream_finish(b, OP).unwrap(), vec![2, 3]);
    }

    #[test]
    fn seam_holds_for_the_wired_core() {
        fn assert_backend<T: CoreBackend>(_: &T) {}
        assert_backend(&initialized());
    }

    #[test]
    fn wire_message_names_code_domain_operation() {
        let err = EchoError::bytes_invalid("echo-counter", "u64.input");
        assert_eq!(
            err.wire_message(),
            "bytes.invalid|core|echo-counter|u64.input"
        );
    }

    #[test]
    fn init_constructs_a_real_central() {
        // U7 transition-driving: `init` constructs a REAL Central (owning
        // the one kernel): zero live operations, zero retained cleanup.
        let core = initialized();
        assert_eq!(
            core.central_status("central-status").unwrap(),
            "{\"revision\":\"C-UBM.0.1.1-DRAFT\",\"live_operations\":0,\"retained_cleanup\":0}"
        );
    }

    #[test]
    fn expire_sweep_drives_the_kernel() {
        let mut core = initialized();
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
        assert_eq!(
            (err.code, err.detail),
            (EchoCode::BytesInvalid, "u64.input")
        );
    }

    #[test]
    fn destroy_drives_shutdown_and_is_idempotent() {
        let mut core = initialized();
        assert_eq!(core.drive_destroy("central-destroy").unwrap(), "released");
        assert_eq!(core.drive_destroy("central-destroy").unwrap(), "released");
        // Streams stay usable after the destroy drive (documented split:
        // destroy drives the central, init owns the binding lifetime).
        let handle = core.stream_begin(OP).unwrap();
        assert_eq!(core.stream_finish(handle, OP).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn unwired_transitions_reject_with_capability_unsupported() {
        let core = initialized();
        for transition in ["scan.start", "queue-advertisement", "force-disconnect"] {
            let err = core
                .request_ble_transition(transition, "request-ble-transition")
                .expect_err("unwired transition must reject");
            assert_eq!(
                (err.code, err.name, err.domain, err.operation, err.detail),
                (
                    EchoCode::CapabilityUnsupported,
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
            (err.code, err.name),
            (EchoCode::ArgumentInvalid, "argument.invalid")
        );
    }

    #[test]
    fn driving_fails_closed_before_init() {
        // Lifetime rules preserved: driving before init rejects with
        // `lifecycle.invalid-state` like every other call.
        let mut core = CoreSession::new();
        for err in [
            core.central_status("central-status").expect_err("uninit"),
            core.drive_expire_sweep("0", "central-expire-sweep")
                .expect_err("uninit"),
            // Gate-first ordering: even garbage time reports the lifetime.
            core.drive_expire_sweep("nope", "central-expire-sweep")
                .expect_err("uninit"),
            core.drive_destroy("central-destroy").expect_err("uninit"),
            core.request_ble_transition("scan.start", "request-ble-transition")
                .expect_err("uninit"),
        ] {
            assert_eq!(
                (err.code, err.name),
                (EchoCode::InvalidState, "lifecycle.invalid-state")
            );
        }
    }
}
