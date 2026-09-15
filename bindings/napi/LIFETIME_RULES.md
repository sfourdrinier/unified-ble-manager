# LIFETIME_RULES — bindings/napi (UBM 5.0 FFI feasibility)

Tested build: `napi =2.16.17 + napi4 feature`, `rustc 1.98.1
(48a229cea 2026-09-01)`, Node `v22.21.1`, Linux x86_64, debug cdylib loaded
as `ubm_echo.linux-x64.node`. Proven by `js/roundtrip.cjs`,
`js/exit_probe.cjs`, and `cargo test`. Anything outside this envelope is a
limitation, not a pass.

## Thread / runtime lifetimes

- The addon owns no threads. Async work (`echoBytesAsync`) runs on the
  libuv threadpool via napi `AsyncTask`; the main thread never blocks.
- `EchoSession` state lives behind `std::sync::Mutex` and is `Send`-safe;
  all JS entry runs on the main thread, all compute on pool threads.
- Tested envelope: at most ONE in-flight async unit per session. Concurrent
  same-session async calls are untested and MUST NOT be relied on (the armed
  cancel flag is session-scoped; see cancellation).
- `close()` is idempotent and terminal: it arms cancellation (in-flight work
  reports `operation.aborted`), marks the session destroyed, and aborts the
  callback registration.

## Callback invalidation

- Registration (`onEvent`) uses a napi `ThreadsafeFunction` with the default
  `CalleeHandled` strategy: delivery is Node-style `(err, value)`.
- Re-registering aborts the previous registration first: no double delivery.
- After `close`, the registration slot is empty AND the handle aborted, so
  `emitTestEvent` rejects with `lifecycle.destroyed|core|emit-test-event|…`
  and the JS callback provably receives nothing further (asserted: `seen`
  stays `['hello']`).
- Forcing a callback after client close therefore fails loudly; it can never
  deliver into a dead client silently.

## Cancellation

- Session-scoped armed flag (AbortSignal flavour): `cancelInflight()` arms;
  the next unit of session work reports `operation.aborted` and disarms.
- Cancel-before-dispatch aborts on entry (`cancelled-before-start`);
  mid-flight cancel aborts at the next chunk boundary (`cancelled`).
  Deterministic in every single-flight interleaving; proven by unit tests
  (including a threaded mid-flight abort) and the JS exchange.
- `close()` during flight aborts the pending call with `operation.aborted`;
  later calls reject with `lifecycle.destroyed`.

## Panic containment (tested)

- Every export is `#[napi(catch_unwind)]`. The `__feasibilityPanicProbe`
  (test-only, pinned `js_name`, MUST NOT ship) proves a Rust panic surfaces
  as a JS `Error` and the process survives. Observation: the panic message is
  still printed to stderr (Rust default hook) — containment means no abort,
  not silence. Sync exports throw synchronously; async `Task::compute`
  panics reject the promise (same `catch_unwind` wrapper; not separately
  probed — follow-up if production use is ever considered).

## Byte ownership

- Entry: `Buffer` is borrowed as `&[u8]` and COPIED into an owned `Vec<u8>`
  before any queueing or return. Rust never retains a borrow of JS memory.
- Exit: fresh `Buffer`s are built from owned `Vec<u8>` (`From<Vec<u8>>`,
  ownership transfers to the JS GC). Mutating the caller buffer after the
  call provably leaves prior outputs untouched (asserted).
- u64 counters cross as decimal strings (`BigInt(n).toString()` in,
  `BigInt(out)` out): `2^64-1` round-trips losslessly; `2^64` and garbage
  reject with `bytes.invalid|core|…`. Canonical form strips leading zeros.

## Error identities

- Every rejection message is `code|domain|operation|detail` with frozen
  C-UBM codes (`protocol.incompatible`, `bytes.too-large`, `bytes.invalid`,
  `argument.invalid`, `operation.aborted`, `lifecycle.destroyed`,
  `lifecycle.invalid-state` via `EchoCore::check_usable`,
  `lifecycle.invariant-violation` for poisoned locks).

## Limitations (explicit, not passes)

- Tested on Linux x86_64 / Node 22 only. Other OS/ABI/arch builds
  (Windows/macOS/Electron exact-ABI addons) are unbuilt and explicitly
  blocked until produced on those hosts — no fallback implied.
- `napi4` cargo feature floors the runtime at N-API version 4.
- `__feasibilityPanicProbe` is test-only and must be deleted before any
  production-shaped use; feasibility-only exports never enter production.
- No `ubm-core` wiring yet: `EchoSession` talks to `EchoCore` through the
  `CoreBackend` seam; swapping in the real core touches one `impl`
  (explicit follow-up).
