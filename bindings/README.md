# bindings/ — UBM 5.0 FFI feasibility slice (trackourhealth/bun-mono#1188)

Feasibility, NOT full bindings: echo-only round-trips proving the binding
mechanics each FFI card requires (typed C-UBM errors, owned byte batches,
lossless u64 counters, async/cooperative cancellation, callback/close
invalidation, init contract, panic containment, clean process behaviour).
No BLE functionality is reported from these tests.

## Layout (exclusive to this slice; no `crates/` dependency exists or is used)

| Dir | Card | Proves |
|---|---|---|
| `napi/` | FFI-NAPI | Real `.node` addon: sync/async echo, TSFN callback invalidation, mid-flight + close-during-flight cancel, panic probe, clean exit |
| `wasm/` | FFI-WASM | Zero-import portable module in Node with empty imports; streaming cancel; BigInt/JSON mapping; `js-glue` mapping compile + export proof |
| `uniffi/` | FFI-NATIVE | UDL scaffold; pinned Kotlin/Swift/Python codegen (reproducibility-diffed); 35-check Python exchange through the real scaffolding |
| `jni/` | FFI-NATIVE | Real JVM exchange through JNI: sessions, typed exceptions, threaded cancel, panic containment in-VM |

Each binding owns a `CoreBackend` seam: the surface calls an echo-only
stand-in core ONLY through that trait. Wiring the real `ubm-core` later
touches one `impl` per binding (explicit follow-up; `crates/ubm-core` is
built in a parallel slice and nothing here depends on it).

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

## Gates (per crate, all observed green)

`cargo fmt --check` · `cargo check` · `cargo clippy --all-targets -- -D warnings`
· `cargo test` · `run_*_roundtrip.sh` (real host exchange).

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

## Explicit follow-ups (not done here)

1. Wire each `CoreBackend` seam to the real `ubm-core` handle.
2. Remove test-only panic probes before any production-shaped use.
3. HOST-WEB AbortSignal wrapper over the wasm stream protocol.
4. Android ART + ABI matrix; Wear OS direct-call path; supported-ABI list.
5. Swift/Kotlin consumer compiles (U-APPLE boundary).
6. Re-check `jni` throw semantics on version bump; per-method checksum
   enforcement if uniffi re-enables it.
