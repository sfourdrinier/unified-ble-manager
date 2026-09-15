//! Raw portable ABI: integers and linear memory only, zero imports.
//!
//! A host drives the protocol with nothing but a memory and integer calls,
//! proven by instantiating the built `.wasm` in Node with an EMPTY import
//! object. Every buffer crossing is an explicit copy; Rust never retains a
//! borrow of host memory and the host never views Rust memory without a
//! copy. Returned buffers are host-owned and must be released with
//! [`ubm_echo_free`] exactly once (the JS harness counts allocs/frees).
//!
//! `unsafe` below is confined to this boundary module: each block documents
//! its contract in a `SAFETY` comment. Everything else is safe Rust.

use crate::echo_core::{check_revision, CoreBackend, EchoCode, EchoCore, EchoError};
use std::sync::{Mutex, OnceLock};

fn core() -> &'static Mutex<EchoCore> {
    static CORE: OnceLock<Mutex<EchoCore>> = OnceLock::new();
    CORE.get_or_init(|| Mutex::new(EchoCore::new()))
}

fn set_last_error(err: EchoError) -> EchoCode {
    let code = err.code;
    let text = err.wire_message();
    if let Ok(mut slot) = last_error().lock() {
        *slot = Some(err);
    }
    if let Ok(mut buf) = last_error_text().lock() {
        buf.clear();
        buf.extend_from_slice(text.as_bytes());
    }
    code
}

fn clear_last_error() {
    if let Ok(mut slot) = last_error().lock() {
        *slot = None;
    }
    if let Ok(mut buf) = last_error_text().lock() {
        buf.clear();
    }
}

fn last_error() -> &'static Mutex<Option<EchoError>> {
    static SLOT: OnceLock<Mutex<Option<EchoError>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn last_error_text() -> &'static Mutex<Vec<u8>> {
    static BUF: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock_failed(operation: &'static str) -> EchoError {
    EchoError::new(
        EchoCode::InvalidState,
        "lifecycle.invariant-violation",
        "core",
        operation,
        "lock-poisoned",
    )
}

/// Exposes the shared core to the optional `js-glue` mapping so both
/// surfaces speak one init state. `pub(crate)` only: never a public export.
#[cfg(feature = "js-glue")]
pub(crate) fn with_core<R>(f: impl FnOnce(&mut EchoCore) -> R) -> R {
    let mut guard = core().lock().expect("core lock poisoned");
    f(&mut guard)
}

/// Test-only serialisation: the raw ABI keeps process-global state, so
/// in-process tests hold this lock across a full call sequence.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Allocates `len` zeroed bytes in the module memory and returns the pointer.
/// The host writes exactly `len` bytes, then passes the pointer to exactly
/// one consuming call. Release unused allocations with [`ubm_echo_free`]
/// passing the same `len`. Returns null for out-of-range lengths (empty
/// batches need no allocation: pass a null pointer with length 0).
#[no_mangle]
pub extern "C" fn ubm_echo_alloc(len: usize) -> *mut u8 {
    if len == 0 || len > crate::echo_core::MAX_OPERATION_BYTES {
        return std::ptr::null_mut();
    }
    // Boxed slice: exact-length layout, so ubm_echo_free(ptr, len) reclaims
    // precisely. No capacity bookkeeping crosses the boundary.
    let boxed = vec![0u8; len].into_boxed_slice();
    Box::into_raw(boxed) as *mut u8
}

/// Releases an allocation from [`ubm_echo_alloc`] that was never consumed, or
/// a buffer returned by a call that documents host ownership. `len` MUST be
/// the length the pointer was created with (the alloc size, or the published
/// `out_len`). Each pointer is freed exactly once; the harness counts
/// allocs/frees to uphold this.
#[no_mangle]
///
/// # Safety
///
/// Callers must pass a pointer/length pair created by this module (an alloc or a published return) exactly once.
pub unsafe extern "C" fn ubm_echo_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: ptr/len is an exact Box<[u8]> allocation created above or by
    // publish_owned, passed exactly once, upheld by the harness.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}

/// Reads a host buffer. Zero lengths short-circuit to empty WITHOUT touching
/// the pointer (no allocation needed for empty batches); null pointers and
/// over-cap lengths are loud `argument.invalid` rejections, never UB.
fn read_host(ptr: *const u8, len: usize, operation: &'static str) -> Result<Vec<u8>, EchoError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(EchoError::argument_invalid(operation, "null-pointer"));
    }
    if len > crate::echo_core::MAX_OPERATION_BYTES {
        return Err(EchoError::argument_invalid(operation, "length-range"));
    }
    // SAFETY: non-null + bounded length per the host contract; the host
    // guarantees the range is mapped and immutable for the call. Copied
    // immediately; no borrow escapes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    Ok(bytes.to_vec())
}

/// Publishes a Rust-owned buffer to the host. The host MUST copy it out
/// immediately, then pass the returned pointer to [`ubm_echo_free`] with the
/// reported length exactly once. Empty results report length 0 with a null
/// pointer the host MUST NOT free.
fn publish_owned(bytes: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    if out_len.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: out_len was checked non-null; the host guarantees writability.
    unsafe {
        *out_len = bytes.len();
    }
    if bytes.is_empty() {
        return std::ptr::null_mut();
    }
    Box::into_raw(bytes.into_boxed_slice()) as *mut u8
}

/// Initialises the binding (PKG-02 / WEB init contract). Must precede every
/// other call except allocation helpers and error inspection. A foreign
/// revision fails closed with `protocol.incompatible`; re-init with a
/// different revision after success also fails. Returns an [`EchoCode`].
#[no_mangle]
///
/// # Safety
///
/// When `rev_len > 0`, `rev_ptr` must span `rev_len` readable bytes.
pub unsafe extern "C" fn ubm_echo_init(rev_ptr: *const u8, rev_len: usize) -> u32 {
    let op = "echo-init";
    let revision = match read_host(rev_ptr, rev_len, op) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return set_last_error(EchoError::bytes_invalid(op, "revision-utf8")) as u32,
        },
        Err(err) => return set_last_error(err) as u32,
    };
    let check = check_revision(&revision, op);
    if check.is_err() {
        return set_last_error(check.expect_err("checked")) as u32;
    }
    let mut guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => return set_last_error(lock_failed(op)) as u32,
    };
    match guard.init(&revision) {
        Ok(()) => {
            clear_last_error();
            EchoCode::Ok as u32
        }
        Err(err) => set_last_error(err) as u32,
    }
}

/// Synchronous owned byte-batch echo. Returns null on error (see
/// [`ubm_echo_last_error`]); otherwise a host-owned buffer published per
/// [`publish_owned`]. Empty input echoes as length 0 (null return, no error).
#[no_mangle]
///
/// # Safety
///
/// When `in_len > 0`, `in_ptr` must span `in_len` readable bytes; `out_len` must be writable.
pub unsafe extern "C" fn ubm_echo_run(
    in_ptr: *const u8,
    in_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let op = "echo-bytes";
    if out_len.is_null() {
        set_last_error(EchoError::argument_invalid(op, "null-out-len"));
        return std::ptr::null_mut();
    }
    if in_len == 0 {
        // SAFETY: out_len checked non-null above.
        unsafe {
            *out_len = 0;
        }
        let guard = match core().lock() {
            Ok(guard) => guard,
            Err(_) => {
                set_last_error(lock_failed(op));
                return std::ptr::null_mut();
            }
        };
        return match CoreBackend::echo_bytes(&*guard, &[], op) {
            Ok(_) => {
                clear_last_error();
                std::ptr::null_mut()
            }
            Err(err) => {
                set_last_error(err);
                std::ptr::null_mut()
            }
        };
    }
    let input = match read_host(in_ptr, in_len, op) {
        Ok(bytes) => bytes,
        Err(err) => {
            set_last_error(err);
            return std::ptr::null_mut();
        }
    };
    let guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error(lock_failed(op));
            return std::ptr::null_mut();
        }
    };
    match CoreBackend::echo_bytes(&*guard, &input, op) {
        Ok(out) => {
            clear_last_error();
            drop(guard);
            publish_owned(out, out_len)
        }
        Err(err) => {
            set_last_error(err);
            std::ptr::null_mut()
        }
    }
}

/// Lossless u64 echo over decimal UTF-8 (DATA-02 BigInt mapping: the host
/// passes `BigInt(n).toString()` bytes and converts the result back with
/// `BigInt(text)`). Returns null on error; otherwise a host-owned buffer.
#[no_mangle]
///
/// # Safety
///
/// When `in_len > 0`, `in_ptr` must span `in_len` readable bytes; `out_len` must be writable.
pub unsafe extern "C" fn ubm_echo_counter(
    in_ptr: *const u8,
    in_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let op = "echo-counter";
    if out_len.is_null() {
        set_last_error(EchoError::argument_invalid(op, "null-out-len"));
        return std::ptr::null_mut();
    }
    let input = match read_host(in_ptr, in_len, op) {
        Ok(bytes) => bytes,
        Err(err) => {
            set_last_error(err);
            return std::ptr::null_mut();
        }
    };
    let decimal = match String::from_utf8(input) {
        Ok(text) => text,
        Err(_) => {
            set_last_error(EchoError::bytes_invalid(op, "u64.input"));
            return std::ptr::null_mut();
        }
    };
    let guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error(lock_failed(op));
            return std::ptr::null_mut();
        }
    };
    match CoreBackend::echo_counter(&*guard, &decimal, op) {
        Ok(canonical) => {
            clear_last_error();
            drop(guard);
            publish_owned(canonical.into_bytes(), out_len)
        }
        Err(err) => {
            set_last_error(err);
            std::ptr::null_mut()
        }
    }
}

/// Numeric code of the last failed call on this thread (`0` = no error).
#[no_mangle]
pub extern "C" fn ubm_echo_last_error() -> u32 {
    last_error()
        .lock()
        .map(|slot| slot.as_ref().map(|err| err.code as u32).unwrap_or(0))
        .unwrap_or(EchoCode::InvalidState as u32)
}

/// Full typed identity of the last failure as `code|domain|operation|detail`
/// UTF-8. Valid until the next binding call; the host copies it out
/// immediately and MUST NOT free it.
#[no_mangle]
///
/// # Safety
///
/// `out_len` must be writable.
pub unsafe extern "C" fn ubm_echo_last_error_text(out_len: *mut usize) -> *const u8 {
    if out_len.is_null() {
        return std::ptr::null();
    }
    let guard = match last_error_text().lock() {
        Ok(guard) => guard,
        Err(_) => return std::ptr::null(),
    };
    // SAFETY: out_len checked non-null; buffer lives in the static slot.
    unsafe {
        *out_len = guard.len();
    }
    if guard.is_empty() {
        return std::ptr::null();
    }
    guard.as_ptr()
}

/// Opens a streaming echo; returns the handle, or 0 on error (uninitialised
/// binding). Cancellation: [`ubm_echo_stream_cancel`], completion:
/// [`ubm_echo_stream_finish`].
#[no_mangle]
pub extern "C" fn ubm_echo_stream_begin() -> u64 {
    let op = "echo-stream-begin";
    let mut guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error(lock_failed(op));
            return 0;
        }
    };
    match guard.stream_begin(op) {
        Ok(handle) => {
            clear_last_error();
            handle
        }
        Err(err) => {
            set_last_error(err);
            0
        }
    }
}

/// Pushes a chunk (empty chunks are valid no-ops); returns the accumulated
/// length, or `usize::MAX` on error.
#[no_mangle]
///
/// # Safety
///
/// When `len > 0`, `ptr` must span `len` readable bytes.
pub unsafe extern "C" fn ubm_echo_stream_push(handle: u64, ptr: *const u8, len: usize) -> usize {
    let op = "echo-stream-push";
    let chunk = match read_host(ptr, len, op) {
        Ok(bytes) => bytes,
        Err(err) => {
            set_last_error(err);
            return usize::MAX;
        }
    };
    let mut guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error(lock_failed(op));
            return usize::MAX;
        }
    };
    match guard.stream_push(handle, &chunk, op) {
        Ok(total) => {
            clear_last_error();
            total
        }
        Err(err) => {
            set_last_error(err);
            usize::MAX
        }
    }
}

/// Cancels a stream; returns an [`EchoCode`]. A cancelled stream finishes
/// with `operation.aborted` exactly once.
#[no_mangle]
pub extern "C" fn ubm_echo_stream_cancel(handle: u64) -> u32 {
    let op = "echo-stream-cancel";
    let mut guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => return set_last_error(lock_failed(op)) as u32,
    };
    match guard.stream_cancel(handle, op) {
        Ok(()) => {
            clear_last_error();
            EchoCode::Ok as u32
        }
        Err(err) => set_last_error(err) as u32,
    }
}

/// Finishes a stream, consuming the handle. Returns null on error (cancelled
/// streams report `operation.aborted`); otherwise a host-owned buffer per
/// [`publish_owned`]. Empty streams finish as length 0.
#[no_mangle]
///
/// # Safety
///
/// `out_len` must be writable.
pub unsafe extern "C" fn ubm_echo_stream_finish(handle: u64, out_len: *mut usize) -> *mut u8 {
    let op = "echo-stream-finish";
    if out_len.is_null() {
        set_last_error(EchoError::argument_invalid(op, "null-out-len"));
        return std::ptr::null_mut();
    }
    let mut guard = match core().lock() {
        Ok(guard) => guard,
        Err(_) => {
            set_last_error(lock_failed(op));
            return std::ptr::null_mut();
        }
    };
    match guard.stream_finish(handle, op) {
        Ok(data) => {
            clear_last_error();
            drop(guard);
            publish_owned(data, out_len)
        }
        Err(err) => {
            set_last_error(err);
            std::ptr::null_mut()
        }
    }
}

/// JSON bridge shape for host mapping (`JSON.parse`): revision identity,
/// byte cap, and u64 range in one typed document. Static metadata: callable
/// without init (like a version query, never an effect).
#[no_mangle]
///
/// # Safety
///
/// `out_len` must be writable.
pub unsafe extern "C" fn ubm_echo_describe_json(out_len: *mut usize) -> *mut u8 {
    let op = "echo-describe";
    if out_len.is_null() {
        set_last_error(EchoError::argument_invalid(op, "null-out-len"));
        return std::ptr::null_mut();
    }
    let doc = format!(
        "{{\"revision\":\"{}\",\"maxBytes\":{},\"u64max\":\"{}\"}}",
        crate::echo_core::CONTRACT_REVISION,
        crate::echo_core::MAX_OPERATION_BYTES,
        crate::echo_core::U64_MAX_DECIMAL
    );
    clear_last_error();
    publish_owned(doc.into_bytes(), out_len)
}

/// Panic probe: traps the module. The host must observe a catchable
/// `WebAssembly.RuntimeError`, never a host crash. Test-only.
#[no_mangle]
pub extern "C" fn ubm_echo_panic_probe() -> u32 {
    panic!("feasibility panic probe: must trap catchably, never crash the host");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::echo_core::CONTRACT_REVISION;

    /// Calls the raw ABI the way a host does: bytes in, copied bytes out,
    /// every allocation freed exactly once.
    struct Host {
        // Held for the whole test: the ABI keeps process-global state, and
        // tests run in parallel threads.
        _serial: std::sync::MutexGuard<'static, ()>,
        allocs: usize,
        freed: Vec<(usize, usize)>,
    }

    impl Host {
        fn new() -> Self {
            let serial = TEST_LOCK.lock().unwrap();
            Self {
                _serial: serial,
                allocs: 0,
                freed: Vec::new(),
            }
        }

        fn write(&mut self, bytes: &[u8]) -> (*mut u8, usize) {
            let len = bytes.len().max(1);
            self.allocs += 1;
            let ptr = ubm_echo_alloc(len);
            assert!(!ptr.is_null(), "alloc failed");
            // SAFETY: just allocated `len` bytes; the test owns them.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            }
            (ptr, len)
        }

        fn free(&mut self, ptr: *mut u8, len: usize) {
            if ptr.is_null() {
                return;
            }
            // SAFETY: ptr/len came from Host::write or a published return and
            // is freed exactly once (freed ledger), upholding the callee contract.
            unsafe {
                ubm_echo_free(ptr, len);
            }
            self.freed.push((ptr as usize, len));
        }

        fn init(&mut self) {
            let (ptr, len) = self.write(CONTRACT_REVISION.as_bytes());
            // SAFETY: freshly allocated, in-bounds, valid UTF-8 revision text.
            assert_eq!(unsafe { ubm_echo_init(ptr, CONTRACT_REVISION.len()) }, 0);
            self.free(ptr, len);
        }

        fn last_text(&mut self) -> String {
            let mut len = 0usize;
            // SAFETY: &mut len is a valid writable out-param; the returned
            // text is copied out before the next call.
            let ptr = unsafe { ubm_echo_last_error_text(&mut len) };
            assert!(!ptr.is_null() && len > 0);
            // SAFETY: valid until the next call; copied out immediately.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            String::from_utf8(bytes.to_vec()).unwrap()
        }
    }

    #[test]
    fn abi_round_trip_copies_and_frees() {
        let mut host = Host::new();
        host.init();
        let (ptr, cap) = host.write(&[10, 20, 30]);
        let mut out_len = 0usize;
        // SAFETY: ptr spans 3 readable bytes; out_len is writable.
        let out = unsafe { ubm_echo_run(ptr, 3, &mut out_len) };
        host.free(ptr, cap);
        assert_eq!(out_len, 3);
        assert!(!out.is_null());
        // SAFETY: published buffer, capacity == length after shrink-to-fit.
        let bytes = unsafe { std::slice::from_raw_parts(out, out_len) };
        assert_eq!(bytes, &[10, 20, 30]);
        host.free(out, out_len);
        assert_eq!(ubm_echo_last_error(), 0);
        // Two allocs (revision + input), three frees (revision + input +
        // published output): every pointer freed exactly once, none leaked.
        assert_eq!(host.allocs, 2);
        assert_eq!(host.freed.len(), 3);
    }

    #[test]
    fn abi_rejects_null_loudly() {
        // Fresh-instance (never initialised) behaviour is proven by the Node
        // exchange test, which starts a pristine module: the in-process
        // global may already be initialised by a sibling test.
        let mut host = Host::new();
        host.init();
        let mut out_len = 0usize;
        // SAFETY: null input with nonzero length is the documented loud-error
        // case; out_len is writable. Never dereferenced on this path.
        assert!(unsafe { ubm_echo_run(std::ptr::null(), 3, &mut out_len) }.is_null());
        assert_eq!(ubm_echo_last_error(), EchoCode::ArgumentInvalid as u32);
        assert!(host
            .last_text()
            .starts_with("argument.invalid|core|echo-bytes|"));
        // Unknown stream handles reject loudly, never alias stream 0.
        let mut len = 0usize;
        // SAFETY: no pointer input; len is writable.
        assert!(unsafe { ubm_echo_stream_finish(999_999, &mut len) }.is_null());
        assert_eq!(ubm_echo_last_error(), EchoCode::InvalidState as u32);
    }
}
