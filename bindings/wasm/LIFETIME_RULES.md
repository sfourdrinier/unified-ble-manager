# LIFETIME_RULES — bindings/wasm (UBM 5.0 FFI feasibility)

Tested builds: default `wasm32-unknown-unknown` on `rustc 1.98.1
(48a229cea 2026-09-01)` (zero imports; the only dependency is the portable
`ubm-core`), driven in Node `v22.21.1` via `js/roundtrip.mjs`; `js-glue`
(`wasm-bindgen =0.2.128`) compile-checked for wasm32 plus export presence.
Contract `C-UBM.0.1.1-DRAFT`, single-owned by `ubm-core` (workspace member).
Proven by `cargo test` and `run_wasm_roundtrip.sh`. Anything outside this
envelope is a limitation, not a pass.

## Core wiring (UBM 5.0 wiring slice)

- `CoreBackend` is implemented for the ubm-core-backed `CoreSession`
  (`src/core_backend.rs`, the one implementation in this crate). Revision
  identity, the byte ceiling, the u64-max document value, and
  decimal-string counter parsing come from `ubm_core::contracts`; no
  contract constant is duplicated here. The former echo-only stand-in
  (`echo_core.rs`) is deleted — no dual owners. The portable build still
  instantiates with an empty import object (proven on every run), so the
  `ubm-core` link adds no host imports.
- The echo transport itself stays feasibility-echo (NOT BLE functionality);
  wiring real kernel transitions through this seam is later U7 scope.

## Thread / runtime lifetimes

- Single-threaded by construction: `wasm32-unknown-unknown` has no threads
  here, and the default build has zero imports (proven: `WebAssembly.Module.
  imports()` is empty), so no host runtime, scheduler, or JS glue can run
  anything behind the caller's back.
- Module-global state (`CoreSession`, last-error slot/text) lives behind
  `std::sync::Mutex` statics. On this target the mutex never contends; on a
  threaded host build the same code stays data-race-free (locks, never
  `static mut`). Multi-threaded SharedArrayBuffer hosts are untested —
  limitation, not a pass.
- Every call completes synchronously before returning (synchronous
  reentrancy): interleaved streams `a`/`b` prove there is no shared mutable
  cursor — each call is atomic from the host's view.

## Init contract (WEB preloading)

- `ubm_echo_init` must precede every effectful call (PKG-02 / FFI-WASM init
  contract). A fresh instance rejects `run`/`stream_begin` with
  `lifecycle.invalid-state` (proven first, before any init, in the exchange).
- Foreign revision → `protocol.incompatible`; same-revision re-init is a
  no-op success; different-revision re-init after success fails closed and
  does NOT de-initialise the binding.
- `ubm_echo_describe_json` is static metadata (revision/caps document) and
  intentionally callable without init — like a version query, never an effect.

## Callback invalidation

- No callbacks exist on this boundary: completion is synchronous return
  values, cancellation is the explicit stream protocol below. There is no
  registration to invalidate and no dropped-client delivery path — stated so
  the absence is a decision, not a gap. (Async JS wrappers belong to the
  host; the HOST-WEB consumer must map stream cancel onto AbortSignal —
  follow-up.)

## Cancellation

- Cooperative stream protocol: `begin → push* → finish`, `cancel` at any
  point before finish. Push-after-cancel and finish-after-cancel report
  `operation.aborted` exactly once and consume the handle; any further use
  of the consumed handle rejects `lifecycle.invalid-state` — never a silent
  replay or a second abort.
- Unknown/`0` handles: `push`/`finish`/`cancel` reject loudly
  (`argument.invalid` for null, `lifecycle.invalid-state` for unknown), never
  aliasing stream 0 or a live stream.
- Cancellation completion: the abort is reported at the call that observes
  it; no background work exists to outlive the call.

## Panic containment

- `panic = unwind` is NOT relied on: a Rust panic traps the module. The
  former test-only `ubm_echo_panic_probe` was deleted by the wiring slice
  and the exchange asserts its absence (`ex.ubm_echo_panic_probe ===
  undefined`): probes must not ship in production paths, so no live trap
  evidence remains. A panic still poisons in-progress Rust state behind the
  trap (documented institute behaviour); hosts must treat a trap as
  session-fatal for the in-flight call and re-drive from the last
  acknowledged state — follow-up for the HOST-WEB consumer contract.

## Byte ownership

- Entry: the host allocates with `ubm_echo_alloc(len)` (exact-length
  `Box<[u8]>` layout), writes `len` bytes, and the call copies them
  immediately (`read_host`). Rust never retains a borrow of host memory.
  Empty inputs cross as `(null, 0)` with no allocation and no dereference.
- Exit: results are published as fresh module buffers; the host copies out
  via the single `takePublished` choke point, then releases with
  `ubm_echo_free(ptr, out_len)` exactly once. Null returns (empty results,
  errors) are never freed.
- Proven: per-kind live accounting (no double-free, balanced allocs) plus a
  leak probe — 100 identical 4 KiB ownership cycles leave
  `memory.buffer.byteLength` bit-identical (freed blocks reused; a leak
  would add ~800 KiB).
- Unsafe is confined to this boundary (`raw_abi.rs` only): every
  pointer-taking export is `unsafe extern "C"` with a `# Safety` section;
  every `unsafe` block carries a `SAFETY` comment. Everything else is safe
  Rust.

## Error identities

- Numeric [`EchoCode`] for integer-only hosts (`0` ok … `6`
  protocol.incompatible) plus the full typed identity via
  `ubm_echo_last_error_text` as `code|domain|operation|detail` (same wire
  form as every other binding; static slot, valid until the next call,
  copied — never freed — by the host).

## BigInt / JSON mapping

- u64 counters cross as decimal UTF-8: host passes
  `BigInt(n).toString()`, converts back with `BigInt(text)`. `2^64-1`
  round-trips losslessly; `2^64`, negatives, and garbage reject
  `bytes.invalid`. Canonical form strips leading zeros.
- `ubm_echo_describe_json` yields the bridge document
  (`revision`/`maxBytes`/`u64max`) for `JSON.parse` — proven parsed and
  asserted, including `BigInt(doc.u64max)`.
- `js-glue` maps the same semantics to `Uint8Array`/`string`/`JsValue`
  (wire-string rejections): compile-proven for wasm32 with mapped export
  names present (`echoBytes`, `echoCounterU64`, `initContract`,
  `describeJson`). In-process unit tests are impossible for wasm-bindgen
  shims (they abort outside a module instance) — stated, not skipped: the
  conversion logic is the unit-tested `core_backend`, the shims add types only.

## Limitations (explicit, not passes)

- Tested in Node 22 only; browser/WebBluetooth hosts (chooser gestures,
  background/service-worker limits) are HOST-WEB scope, not proven here —
  no peripheral or background promise is made.
- No async JS wrapper ships: cancellation maps to streams, not AbortSignal.
- `ubm-core` wiring is DONE (see above): the surface calls the core ONLY
  through the `CoreBackend` seam; deeper kernel-transition wiring later
  touches the one `impl`.
