//! UniFFI echo scaffold: the exact Rust surface the UDL exposes.
//!
//! The binding talks to the feasibility core ONLY through `CoreBackend`
//! (same seam as every other binding). Failures cross as result records
//! carrying the frozen C-UBM wire form — see the UDL header note.

// Allowed: generated scaffolding (`ubm_echo.uniffi.rs`, included below)
// defines a large metadata const that trips `large_const_arrays`. The lint
// fires only on generated code; nothing else in this crate is affected.
#![allow(clippy::large_const_arrays)]

mod echo_core;

use echo_core::{EchoCore, EchoError, SharedCore};

uniffi::include_scaffolding!("ubm_echo");

/// Mirrors UDL `dictionary EchoBytesResult` (converters generated from UDL).
pub struct EchoBytesResult {
    pub ok: bool,
    pub data: Vec<u8>,
    pub code: String,
    pub domain: String,
    pub operation: String,
}

/// Mirrors UDL `dictionary EchoCounterResult` (converters generated from UDL).
pub struct EchoCounterResult {
    pub ok: bool,
    pub value: String,
    pub code: String,
    pub domain: String,
    pub operation: String,
}

/// Mirrors UDL `dictionary EchoStatus` (converters generated from UDL).
pub struct EchoStatus {
    pub ok: bool,
    pub code: String,
    pub domain: String,
    pub operation: String,
}

fn ok_bytes(data: Vec<u8>) -> EchoBytesResult {
    EchoBytesResult {
        ok: true,
        data,
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
    }
}

fn err_bytes(err: EchoError) -> EchoBytesResult {
    EchoBytesResult {
        ok: false,
        data: Vec::new(),
        code: err.code.to_string(),
        domain: err.domain.to_string(),
        operation: err.operation.to_string(),
    }
}

fn ok_counter(value: String) -> EchoCounterResult {
    EchoCounterResult {
        ok: true,
        value,
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
    }
}

fn err_counter(err: EchoError) -> EchoCounterResult {
    EchoCounterResult {
        ok: false,
        value: String::new(),
        code: err.code.to_string(),
        domain: err.domain.to_string(),
        operation: err.operation.to_string(),
    }
}

fn ok_status() -> EchoStatus {
    EchoStatus {
        ok: true,
        code: String::new(),
        domain: String::new(),
        operation: String::new(),
    }
}

fn err_status(err: EchoError) -> EchoStatus {
    EchoStatus {
        ok: false,
        code: err.code.to_string(),
        domain: err.domain.to_string(),
        operation: err.operation.to_string(),
    }
}

/// Mirrors UDL `interface EchoSession` (scaffolding generated from UDL).
pub struct EchoSession {
    inner: SharedCore,
}

impl EchoSession {
    /// Opens a session. Never fails at construction: the revision gate runs
    /// on every method instead, so a foreign revision fails closed with
    /// `protocol.incompatible` on every call (no effect without valid init).
    pub fn new(revision: String) -> Self {
        Self {
            inner: EchoCore::open(&revision).into_shared(),
        }
    }

    pub fn echo_bytes(&self, input: Vec<u8>) -> EchoBytesResult {
        match self.inner.echo_bytes(&input) {
            Ok(data) => ok_bytes(data),
            Err(err) => err_bytes(err),
        }
    }

    pub fn echo_counter(&self, decimal: String) -> EchoCounterResult {
        match self.inner.echo_counter(&decimal) {
            Ok(value) => ok_counter(value),
            Err(err) => err_counter(err),
        }
    }

    pub fn echo_bytes_chunked(&self, input: Vec<u8>, chunks: u32) -> EchoBytesResult {
        match self.inner.echo_bytes_chunked(&input, chunks) {
            Ok(data) => ok_bytes(data),
            Err(err) => err_bytes(err),
        }
    }

    pub fn cancel_inflight(&self) -> EchoStatus {
        match self.inner.cancel_inflight() {
            Ok(()) => ok_status(),
            Err(err) => err_status(err),
        }
    }

    pub fn close(&self) -> EchoStatus {
        self.inner.close();
        ok_status()
    }

    /// Test-only panic probe: proves the probe panics (in-process test
    /// asserts the unwind); containment at the generated boundary is
    /// evidenced by scaffolding inspection (see LIFETIME_RULES.md).
    /// MUST NOT ship in any production binding.
    pub fn panic_probe(&self) -> EchoStatus {
        panic!("feasibility panic probe: scaffolding must contain this unwind");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV: &str = echo_core::CONTRACT_REVISION;

    #[test]
    fn udl_surface_round_trips() {
        let session = EchoSession::new(REV.to_string());
        let out = session.echo_bytes(vec![1, 2, 250]);
        assert!(out.ok && out.data == vec![1, 2, 250]);
        let counter = session.echo_counter("18446744073709551615".to_string());
        assert!(counter.ok && counter.value == "18446744073709551615");
        let bad = session.echo_counter("nope".to_string());
        assert!(!bad.ok && bad.code == "bytes.invalid" && bad.domain == "core");
    }

    #[test]
    fn udl_surface_gates_revision_and_close() {
        let foreign = EchoSession::new("C-UBM.9.9.9-DRAFT".to_string());
        assert!(!foreign.echo_bytes(vec![1]).ok);
        assert_eq!(foreign.echo_bytes(vec![1]).code, "protocol.incompatible");
        let session = EchoSession::new(REV.to_string());
        assert!(session.close().ok);
        let after = session.echo_bytes(vec![1]);
        assert!(!after.ok && after.code == "lifecycle.destroyed");
    }

    #[test]
    fn udl_surface_cancels_chunked_work() {
        let session = EchoSession::new(REV.to_string());
        assert!(session.cancel_inflight().ok);
        let out = session.echo_bytes_chunked(vec![1, 2, 3], 10);
        assert!(!out.ok && out.code == "operation.aborted");
    }

    #[test]
    fn panic_probe_is_live() {
        let session = EchoSession::new(REV.to_string());
        let probed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.panic_probe();
        }));
        assert!(
            probed.is_err(),
            "probe must panic so containment is meaningful"
        );
    }
}
