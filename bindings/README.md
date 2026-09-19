# bindings/ — UBM 5.0 FFI feasibility slice (trackourhealth/bun-mono#1188)

Feasibility, NOT full bindings: echo-only round-trips proving the binding
mechanics each FFI card requires (typed C-UBM errors, owned byte batches,
lossless u64 counters, async/cooperative cancellation, callback/close
invalidation, init contract, panic containment, clean process behaviour).
No BLE functionality is reported from these tests.

## Layout (exclusive to this slice; wired to `crates/ubm-core`)

Each binding links `ubm-core` as a workspace path dependency and implements
its `CoreBackend` seam for the ubm-core-backed `CoreSession`
(`src/core_backend.rs`, one implementation per binding). Contract truth —
revision `C-UBM.0.1.2-DRAFT`, `MAX_OPERATION_BYTES`, decimal-string counter
parsing — is single-owned by `ubm-core`; the former echo-only stand-ins
(`echo_core.rs`) are deleted, so there are no dual owners. The echo
transport itself stays feasibility-echo (NOT BLE functionality); wiring real
kernel transitions through these seams is later U7 scope. All four binding
crates are members of the root workspace with one unified `Cargo.lock`.

| Dir | Card | Proves |
|---|---|---|
| `napi/` | FFI-NAPI | Real `.node` addon: sync/async echo, TSFN callback invalidation, mid-flight + close-during-flight cancel, clean exit (probe deleted; absence asserted) |
| `wasm/` | FFI-WASM | Zero-import portable module in Node with empty imports; streaming cancel; BigInt/JSON mapping; `js-glue` mapping compile + export proof (probe deleted; absence asserted) |
| `uniffi/` | FFI-NATIVE | UDL scaffold; pinned Kotlin/Swift/Python codegen (reproducibility-diffed); 34-check Python exchange through the real scaffolding (probe deleted; absence asserted) |
| `jni/` | FFI-NATIVE | Real JVM exchange through JNI: sessions, typed exceptions, threaded cancel (probe deleted; bridge policy retained) |

Each binding owns a `CoreBackend` seam: the surface calls the ubm-core-backed
`CoreSession` ONLY through that trait (one `impl` per binding; deeper
kernel-transition wiring later touches that `impl`, not every call site).

Each binding has `LIFETIME_RULES.md` (thread/runtime lifetimes, callback
invalidation, panic containment per tested build, byte ownership) and a
`run_*_roundtrip.sh` gate script. Unimplemented paths reject loudly
everywhere; Swift/Kotlin consumer compiles are a U-APPLE boundary
regardless (recorded, not attempted).

## Toolchain (exact)

- `rustc 1.98.1 (48a229cea 2026-09-01)` / `cargo 1.98.1 (797e8a9bc 2026-08-05)`,
  repo-scoped via `../rust-toolchain.toml`; global default untouched.
- `wasm32-unknown-unknown` target on the 1.98.1 toolchain.
- Node `v22.21.1`; OpenJDK `21.0.12`; Python `3.10.10`.

## Pinned dependencies (exact, minimal per crate)

- napi: `napi =2.16.17` (+`napi4` feature), `napi-derive =2.16.13`, `napi-build =2.4.2`
- wasm: `wasm-bindgen =0.2.128` (optional `js-glue` only; default build zero-dep)
- uniffi: `uniffi =0.32.1` (+`build` feature); codegen `uniffi-bindgen 0.32.1`
- jni: `jni =0.22.4`

## Gates (workspace, all observed green)

`cargo fmt --check` · `cargo check --locked` (workspace) ·
`cargo clippy --all-targets -- -D warnings` (workspace) ·
`cargo test --locked` (workspace) · `wasm32` check for `ubm-core` ·
`run_*_roundtrip.sh` (real host exchange, one per binding).

## Notable findings

- `jni` 0.22.4: `Env::throw` / `throw_new` report `Err` after successfully
  throwing (inverted success logic, proven via pending-exception state).
  The bridge keys throw success on `exception_check`, never the return
  value. Re-check on any `jni` bump.
- uniffi 0.32 Python output leaves the per-method checksum call commented
  out; the enforced mismatch axis is the contract version (negatively
  proven). Swift/Kotlin emitters are recipe-only here.
- napi `CalleeHandled` TSFN delivers `(err, value)`; wasm i32 results
  arrive signed in JS; wasm-bindgen shims abort outside a module instance
  (all recorded in the respective rules).

## Explicit follow-ups

1. DONE (wiring slice): each `CoreBackend` seam is implemented for the
   ubm-core-backed `CoreSession`; echo stand-ins deleted.
2. DONE (wiring slice): test-only panic probes deleted from all production
   paths (absence asserted by every exchange); containment mechanisms
   recorded per binding in `LIFETIME_RULES.md`.
3. HOST-WEB AbortSignal wrapper over the wasm stream protocol.
4. Android ART + ABI matrix; Wear OS direct-call path; supported-ABI list.
5. Swift/Kotlin consumer compiles (U-APPLE boundary).
6. Re-check `jni` throw semantics on version bump; per-method checksum
   enforcement if uniffi re-enables it.
7. Pending contract freeze (recorded here; `contracts/**` stays untouched):
   the wire codec introduces two wire-layer operation strings with no frozen
   TS counterpart — `cleanup.wire.input` (oversize `bytes.too-large` plus
   every malformed-shape `protocol.malformed|boundary` except an empty
   operation id) and `cleanup.wire.operation-id` (empty `operationId`
   string). Both are consistently `cleanup.wire.*`-namespaced. They must be
   frozen at contract acceptance so a future TS validator does not treat
   them as foreign operations.
