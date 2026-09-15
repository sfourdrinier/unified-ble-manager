# LIFETIME_RULES — bindings/uniffi (UBM 5.0 FFI feasibility)

Tested build: `uniffi =0.32.1 + build feature`, `rustc 1.98.1
(48a229cea 2026-09-01)`, Linux x86_64; codegen `uniffi-bindgen 0.32.1`;
Python `3.10.10` driving the REAL generated `ubm_echo.py` against the REAL
built cdylib. Proven by `cargo test` and `run_uniffi_roundtrip.sh`
(35 foreign checks). Anything outside this envelope is a limitation.

## Thread / runtime lifetimes

- `EchoSession` is a UniFFI `Object` (reference-counted, `Send + Sync`
  via an internal `Mutex<EchoCore>`). Foreign threads may share one session;
  the table lock is never held across the chunked worker, so `cancel`/`close`
  from another thread stay effective mid-call (proven by the threaded
  mid-flight abort through the real scaffolding).
- Tested envelope: one long chunked call per session at a time. Concurrent
  same-session chunked calls are untested (session-scoped armed flag).
- The `with_env`-style runtime does not apply here; UniFFI scaffolding owns
  call dispatch. No background threads are created by this crate.
- `close()` is idempotent and terminal: destroys the core (arming
  cancellation for in-flight holders) and every later call reports
  `lifecycle.destroyed`.

## Callback invalidation

- No callbacks exist on this boundary (UDL has no callback interfaces):
  completion is synchronous return records. There is no registration to
  invalidate and no dropped-client delivery path — stated as a decision.
  Streaming/event callbacks for Swift/Kotlin consumers are a follow-up that
  must re-prove invalidation per language.

## Init contract (PKG-02)

- The UDL constructor cannot fail, so the revision gate runs on EVERY
  method: a foreign revision fails closed with `protocol.incompatible` on
  every call (proven for bytes/counter/chunked/cancel). No effect without
  valid init. A production binding would layer `[Throws]` construction on
  top; the fail-closed property is what is proven here.

## Cancellation

- Session-scoped armed flag (same semantics as every binding):
  `cancel_inflight` arms; the next chunked unit reports `operation.aborted`
  (entry-take when the worker has not started, boundary check when it has)
  and disarms, so the session stays usable. Proven in-process (threaded)
  and foreign (Python thread + ctypes GIL release).

## Panic containment (proven through the real scaffolding)

- The `panic_probe` method panics. Called from Python through the generated
  scaffolding, the process SURVIVES and Python receives a UniFFI error
  (`InternalError`); a fresh session stays usable afterwards. Containment
  lives in `uniffi_core::ffi::rustcalls::rust_call` (`catch_unwind` at the
  pinned version) — evidenced by source at the pinned version AND executed
  end-to-end, not merely inspected.
- Observation: like all bindings, the panic message still reaches stderr
  via the Rust default hook — containment means no abort, not silence.

## Byte ownership

- UDL `bytes` map to owned `Vec<u8>` on both sides; UniFFI serialises across
  the boundary, so no borrows escape in either direction (proven by
  512 KiB round-trips plus empty-batch edges through Python).
- u64 counters cross as decimal strings; Python's arbitrary-precision ints
  make the DATA-02 proof exact (`2^64-1` round-trips; `2^64` rejects
  `bytes.invalid`). Canonical form strips leading zeros.

## Error identities

- Failures cross as result records (`ok` + `code` + `domain` + `operation`)
  with frozen C-UBM identities — a deliberate feasibility mapping (see the
  UDL header): a narrowed `[Throws]` enum would lose the exact contract
  identity, while records preserve it verbatim. The Python test rebuilds
  `code|domain|operation` from LIVE record fields and asserts exact
  literals. Records carry no `detail` member in this mapping (limitation,
  recorded): the Rust-side detail stays behind; a production `[Throws]`
  design must carry it (follow-up).

## Codegen / runtime contract (FFI-NATIVE recipe)

- `uniffi-bindgen 0.32.1` generates Kotlin + Swift + Python from the built
  cdylib. The runner regenerates and DIFFS against the committed
  `generated/` recipe (must be byte-identical): Kotlin, Swift, and Python
  outputs are all reproducibility-pinned.
- Import-time contract-version verification is LIVE in the generated
  bindings and is negatively proven (tampered version → `InternalError` on
  import). The per-method checksum call is commented out in uniffi 0.32
  Python output — recorded as an upstream gap, not covered here.
- Mismatched codegen/runtime therefore rejects loudly; stale bindings
  cannot silently drive a new cdylib (for the version axis; per-method
  drift relies on regeneration discipline + the committed diff gate).

## Limitations (explicit, not passes)

- Swift/Kotlin CONSUMER compiles are a U-APPLE boundary regardless: no
  kotlinc/swiftc exist on this host, and mobile OS/SDK behaviour is
  unproven. Generated sources are recipe artifacts, not compile evidence.
  Python execution proves the shared scaffolding + Rust core, not the
  Swift/Kotlin emitters.
- `panic_probe` is test-only and must be deleted before any
  production-shaped use.
- No `ubm-core` wiring yet: the surface calls the core ONLY through the
  `CoreBackend` seam (explicit follow-up).
- `clippy::large_const_arrays` is allowed crate-wide with justification:
  the lint fires only on the GENERATED metadata const, which cannot be
  fixed in-tree.
