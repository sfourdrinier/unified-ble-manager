//! Echo-only feasibility core for the WASM binding (portable, no threads,
//! no Tokio, no filesystem, no radio).
//!
//! STAND-IN behind the `CoreBackend` seam: this module proves the boundary
//! exchange (owned byte batches, lossless u64 counters, typed C-UBM error
//! identities, init contract, cooperative streaming cancellation). It is NOT
//! BLE functionality.
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

/// Frozen contract revision this feasibility slice speaks.
pub const CONTRACT_REVISION: &str = "C-UBM.0.1.0-DRAFT";

/// Mirror of `MAX_OPERATION_BYTES` (contracts/src/bounds.ts).
pub const MAX_OPERATION_BYTES: usize = 524288;

/// Largest u64 value, decimal form (DATA-02 lossless-counter mapping).
pub const U64_MAX_DECIMAL: &str = "18446744073709551615";

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
        Err(EchoError::incompatible(
            operation,
            "contract-revision.mismatch",
        ))
    }
}

/// Owned byte-batch echo: the input is copied on entry, the output is a fresh
/// allocation. The binding must never retain a borrow of caller memory.
pub fn echo_bytes(input: &[u8], operation: &'static str) -> Result<Vec<u8>, EchoError> {
    if input.len() > MAX_OPERATION_BYTES {
        return Err(EchoError::bytes_too_large(
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
        return Err(EchoError::bytes_invalid(operation, "u64.input"));
    }
    let stripped = decimal.trim_start_matches('0');
    let canonical = if stripped.is_empty() { "0" } else { stripped };
    if canonical.len() > U64_MAX_DECIMAL.len()
        || (canonical.len() == U64_MAX_DECIMAL.len() && canonical > U64_MAX_DECIMAL)
    {
        return Err(EchoError::bytes_invalid(operation, "u64.range"));
    }
    Ok(canonical.to_string())
}

/// Feasibility stand-in core: init state plus a streaming-echo table for
/// cooperative cancellation. Every method fails closed outside its valid
/// lifetime. Single-threaded by construction (WASM has no threads here).
#[derive(Debug, Default)]
pub struct EchoCore {
    initialized_revision: Option<&'static str>,
    next_stream: u64,
    streams: std::collections::HashMap<u64, StreamState>,
}

#[derive(Debug)]
struct StreamState {
    data: Vec<u8>,
    cancelled: bool,
}

impl EchoCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialises the binding. A foreign revision fails closed with
    /// `protocol.incompatible`; re-init with a DIFFERENT revision after a
    /// successful init also fails (binding/core identity must agree, PKG-02).
    /// Re-init with the same revision is a no-op success.
    pub fn init(&mut self, revision: &str) -> Result<(), EchoError> {
        check_revision(revision, "echo-init")?;
        if let Some(current) = self.initialized_revision {
            if current != CONTRACT_REVISION {
                return Err(EchoError::incompatible("echo-init", "identity-changed"));
            }
            return Ok(());
        }
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
        if stream.data.len().saturating_add(chunk.len()) > MAX_OPERATION_BYTES {
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

    const OP: &str = "test-op";

    fn initialized() -> EchoCore {
        let mut core = EchoCore::new();
        core.init(CONTRACT_REVISION).unwrap();
        core
    }

    #[test]
    fn foreign_revision_rejects_closed() {
        let mut core = EchoCore::new();
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
        let mut core = EchoCore::new();
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
    fn seam_holds_for_ubm_core_wiring() {
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
}
