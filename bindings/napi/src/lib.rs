//! N-API binding surface for the UBM 5.0 FFI feasibility slice.
//!
//! Exchange proven here (FFI-NAPI card, DATA-02 / PKG-01 / PKG-02 / PKG-04):
//! typed C-UBM errors, owned byte batches, lossless u64 counters, async
//! cancellation, callback invalidation, and clean process exit.
//!
//! Byte ownership: every `Buffer` crossing is copied into an owned `Vec<u8>`
//! on entry (`AsRef<[u8]>` borrow ends before return) and fresh `Buffer`s are
//! built from owned `Vec<u8>` on exit. Rust never retains a borrow of JS
//! memory; JS never views Rust memory without a copy.
//!
//! Panic containment: every export is `#[napi(catch_unwind)]`, so a Rust panic
//! becomes a rejected JS `Error`, never an abort across the ABI. The former
//! test-only `__feasibilityPanicProbe` was deleted by the wiring slice:
//! probes must not ship in production paths, so containment now rests on the
//! `catch_unwind` attribute present on every export.
//!
//! The binding talks to the core ONLY through `CoreBackend`, implemented for
//! the ubm-core-backed [`core_backend::CoreSession`] (one implementation in
//! this crate; contract truth is single-owned by `ubm-core`).

mod core_backend;

use std::sync::Mutex;

use core_backend::{check_revision, CoreBackend, CoreSession, EchoError};
use napi::bindgen_prelude::{AsyncTask, Buffer, Env, Result, Task};
use napi::threadsafe_function::{
    ThreadSafeCallContext, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi::{Error, Status};
use napi_derive::napi;

/// Encode a typed [`EchoError`] as a JS `Error` whose message carries the
/// frozen `code|domain|operation|detail` wire form.
fn to_napi_error(err: EchoError) -> Error {
    Error::new(Status::GenericFailure, err.wire_message())
}

struct SessionInner {
    core: Mutex<CoreSession>,
    events: Mutex<Option<ThreadsafeFunction<String>>>,
}

/// Feasibility echo session. `open` enforces the init contract (PKG-02):
/// a foreign `CONTRACT_REVISION` fails closed with `protocol.incompatible`.
#[napi]
pub struct EchoSession {
    inner: SessionInner,
}

#[napi]
impl EchoSession {
    #[napi(constructor, catch_unwind)]
    pub fn open(revision: String) -> Result<Self> {
        check_revision(&revision, "echo-session.open").map_err(to_napi_error)?;
        Ok(Self {
            inner: SessionInner {
                core: Mutex::new(CoreSession::open(&revision).map_err(to_napi_error)?),
                events: Mutex::new(None),
            },
        })
    }

    /// Synchronous owned byte-batch round-trip.
    #[napi(catch_unwind)]
    pub fn echo_bytes(&self, input: Buffer) -> Result<Buffer> {
        let owned: Vec<u8> = {
            let core = self.inner.core.lock().map_err(|_| {
                Error::new(
                    Status::GenericFailure,
                    "lifecycle.invariant-violation|core|echo-bytes|lock-poisoned",
                )
            })?;
            CoreBackend::echo_bytes(&*core, input.as_ref(), "echo-bytes").map_err(to_napi_error)?
        };
        Ok(owned.into())
    }

    /// Lossless u64 round-trip over decimal strings (`BigInt(n).toString()`
    /// in, `BigInt(out)` out). Values above `Number.MAX_SAFE_INTEGER` survive.
    #[napi(catch_unwind)]
    pub fn echo_counter(&self, decimal: String) -> Result<String> {
        let core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|echo-counter|lock-poisoned",
            )
        })?;
        CoreBackend::echo_counter(&*core, &decimal, "echo-counter").map_err(to_napi_error)
    }

    /// Async echo on the libuv threadpool. `chunks` is a test hook bounding
    /// the work loop (1..=1_000_000); cancellation observed at any chunk
    /// boundary rejects with `operation.aborted`.
    #[napi(catch_unwind)]
    pub fn echo_bytes_async(
        &self,
        input: Buffer,
        chunks: Option<u32>,
    ) -> Result<AsyncTask<EchoAsyncTask>> {
        let core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|echo-bytes-async|lock-poisoned",
            )
        })?;
        // Fail closed before queueing: destroyed sessions and oversize input
        // reject synchronously instead of settling later. The pre-check
        // shares the worker label `echo-bytes-async` (L3: one async label).
        let owned: Vec<u8> = CoreBackend::echo_bytes(&*core, input.as_ref(), "echo-bytes-async")
            .map_err(to_napi_error)?;
        let task = EchoAsyncTask {
            input: owned,
            chunks: chunks.unwrap_or(64),
            cancel: core.cancel_flag(),
        };
        drop(core);
        Ok(AsyncTask::new(task))
    }

    /// Requests cancellation of in-flight async work started by this session.
    /// After `close` this rejects with `lifecycle.destroyed` like every
    /// other call (M1: uniform post-close cancel with uniffi/JNI).
    #[napi(catch_unwind)]
    pub fn cancel_inflight(&self) -> Result<()> {
        let core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|cancel-inflight|lock-poisoned",
            )
        })?;
        core.cancel_inflight("cancel-inflight")
            .map_err(to_napi_error)?;
        Ok(())
    }

    /// Observes the session-owned REAL Central (U7 transition-driving): the
    /// frozen revision plus live kernel counters as a JSON document.
    /// Rejects with `lifecycle.destroyed` after `close` like every call.
    #[napi(catch_unwind)]
    pub fn central_status(&self) -> Result<String> {
        let core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|central-status|lock-poisoned",
            )
        })?;
        core.central_status("central-status").map_err(to_napi_error)
    }

    /// Drives a REAL kernel expiry sweep of the session-owned central at
    /// host-supplied monotonic time `now_ms_decimal` (decimal string,
    /// DATA-02 mapping, lossless past `Number.MAX_SAFE_INTEGER`). Returns
    /// the settled-operation count as decimal.
    #[napi(catch_unwind)]
    pub fn drive_expire_sweep(&self, now_ms_decimal: String) -> Result<String> {
        let mut core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|central-expire-sweep|lock-poisoned",
            )
        })?;
        core.drive_expire_sweep(&now_ms_decimal, "central-expire-sweep")
            .map(|settled| settled.to_string())
            .map_err(to_napi_error)
    }

    /// Drives the REAL shutdown transition of the session-owned central.
    /// Returns `released` on a clean release, `release-failed` otherwise.
    /// Idempotent. Orthogonal to `close` (the session stays usable for echo
    /// until `close` destroys the binding lifetime).
    #[napi(catch_unwind)]
    pub fn drive_destroy(&self) -> Result<String> {
        let mut core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|central-destroy|lock-poisoned",
            )
        })?;
        core.drive_destroy("central-destroy")
            .map(std::string::ToString::to_string)
            .map_err(to_napi_error)
    }

    /// Loud rejection for BLE transitions beyond the driven slice (scan,
    /// connect, GATT, subscribe, ...): this boundary has no radio/host, so
    /// every named transition fails closed with
    /// `capability.unsupported|capability`, never silently or faked. An
    /// empty name is `argument.invalid`; after `close` this reports
    /// `lifecycle.destroyed` like every call.
    #[napi(catch_unwind)]
    pub fn request_ble_transition(&self, transition: String) -> Result<()> {
        let core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|request-ble-transition|lock-poisoned",
            )
        })?;
        core.request_ble_transition(&transition, "request-ble-transition")
            .map_err(to_napi_error)
    }

    /// Registers the event callback. Registering twice replaces the previous
    /// registration (the old one is aborted first: no double delivery).
    #[napi(catch_unwind)]
    pub fn on_event(&self, env: Env, callback: napi::JsFunction) -> Result<()> {
        let tsfn: ThreadsafeFunction<String> =
            callback.create_threadsafe_function(0, |ctx: ThreadSafeCallContext<String>| {
                ctx.env.create_string(ctx.value.as_str()).map(|v| vec![v])
            })?;
        let mut slot = self.inner.events.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|on-event|lock-poisoned",
            )
        })?;
        if let Some(previous) = slot.take() {
            let _ = previous.abort();
        }
        *slot = Some(tsfn);
        let _ = env;
        Ok(())
    }

    /// Delivers one event to the registered callback. Rejects loudly with
    /// `lifecycle.destroyed` when no live registration exists (notably after
    /// `close`): a callback after client close never fires silently.
    #[napi(catch_unwind)]
    pub fn emit_test_event(&self, payload: String) -> Result<()> {
        let slot = self.inner.events.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|emit-test-event|lock-poisoned",
            )
        })?;
        match slot.as_ref() {
            None => Err(Error::new(
                Status::GenericFailure,
                "lifecycle.destroyed|core|emit-test-event|no-live-callback",
            )),
            Some(tsfn) => {
                if tsfn.aborted() {
                    return Err(Error::new(
                        Status::GenericFailure,
                        "lifecycle.destroyed|core|emit-test-event|callback-aborted",
                    ));
                }
                let status = tsfn.call(Ok(payload), ThreadsafeFunctionCallMode::NonBlocking);
                if status != Status::Ok {
                    return Err(Error::new(
                        Status::GenericFailure,
                        format!(
                            "lifecycle.destroyed|core|emit-test-event|delivery-status-{status:?}"
                        ),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Destroys the session: aborts the callback registration, cancels
    /// in-flight work, and invalidates every later call with
    /// `lifecycle.destroyed`. Idempotent.
    #[napi(catch_unwind)]
    pub fn close(&self) -> Result<()> {
        let mut core = self.inner.core.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|close|lock-poisoned",
            )
        })?;
        core.close();
        drop(core);
        let mut slot = self.inner.events.lock().map_err(|_| {
            Error::new(
                Status::GenericFailure,
                "lifecycle.invariant-violation|core|close|lock-poisoned",
            )
        })?;
        if let Some(tsfn) = slot.take() {
            let _ = tsfn.abort();
        }
        Ok(())
    }
}

/// Threadpool work item for [`EchoSession::echo_bytes_async`].
pub struct EchoAsyncTask {
    input: Vec<u8>,
    chunks: u32,
    cancel: std::sync::Arc<core_backend::CancelFlag>,
}

impl Task for EchoAsyncTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        core_backend::echo_bytes_chunked(&self.input, self.chunks, &self.cancel, "echo-bytes-async")
            .map_err(to_napi_error)
    }

    fn resolve(&mut self, _env: Env, output: Vec<u8>) -> napi::Result<Buffer> {
        Ok(output.into())
    }
}

/// Contract revision this binding speaks (PKG-01 artifact identity),
/// single-owned by `ubm-core`.
#[napi(catch_unwind)]
pub fn echo_revision() -> String {
    core_backend::CONTRACT_REVISION.to_string()
}

/// Maximum byte-batch length (single-owned `MAX_OPERATION_BYTES`).
#[napi(catch_unwind)]
pub fn echo_max_bytes() -> u32 {
    core_backend::MAX_OPERATION_BYTES as u32
}
