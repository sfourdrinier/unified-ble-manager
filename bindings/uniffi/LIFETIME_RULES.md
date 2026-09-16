# LIFETIME_RULES — bindings/uniffi (UBM 5.0 FFI feasibility)

Tested build: `uniffi =0.32.1 + build feature`, `rustc 1.98.1
(48a229cea 2026-09-01)`, Linux x86_64; codegen `uniffi-bindgen 0.32.1`;
Python `3.10.10` driving the REAL generated `ubm_echo.py` against the REAL
built cdylib. Contract `C-UBM.0.1.1-DRAFT`, single-owned by `ubm-core`
(workspace member). Proven by `cargo test` and `run_uniffi_roundtrip.sh`
(58 foreign checks). Anything outside this envelope is a limitation.

## Core wiring (UBM 5.0 wiring slice + U7 transition-driving)

- `CoreBackend` is implemented for the ubm-core-backed `CoreSession`
  (`src/core_backend.rs`, the one implementation in this crate). Revision
  identity, the byte ceiling, and decimal-string counter parsing come from
  `ubm_core::contracts`; no contract constant is duplicated here. The former
  echo-only stand-in (`echo_core.rs`) is deleted — no dual owners.
- U7 transition-driving: `CoreSession` holds a REAL `ubm_core::central::Central`
  (which owns the one scheduling `Kernel`), constructed at `open` for a
  fixed binding-process attachment scope with a completed handshake
  (PKG-02). The UDL constructor cannot fail, so construction failure is
  recorded as an absent core and every driving call fails closed with
  `lifecycle.invariant-violation` (unreachable with the fixed labels, never
  assumed). Driven UDL methods (all gated by the revision/close rules
  below, values in the shared result records):
  - `central_status()` observes the core (frozen revision + live kernel
    counters as JSON; fixed shape across bindings).
  - `drive_expire_sweep(now_ms)` drives a real kernel expiry sweep at
    decimal-string host time (DATA-02 mapping); gate-first ordering keeps
    post-close semantics uniform even for garbage input.
  - `drive_destroy()` drives the real shutdown transition
    (`released` / `release-failed`); idempotent; orthogonal to `close`.
  - `request_ble_transition(transition)` is the loud-rejection path for
    every BLE transition beyond the driven slice:
    `capability.unsupported|capability` (the frozen contract pairing),
    never silent or faked; empty names are `argument.invalid`.
- The echo transport itself stays feasibility-echo (NOT BLE functionality).
  Follow-ups: unique per-instance attachment identity (fixed scope labels
  this slice); surfacing staged kernel effects to a host executor (driven
  batches are bounded and dropped after the call — nothing is staged yet,
  so nothing is lost yet).

## Thread / runtime lifetimes

- `EchoSession` is a UniFFI `Object` (reference-counted, `Send + Sync`
  via an internal `Mutex<CoreSession>`). Foreign threads may share one session;
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

## Panic containment

- The former test-only `panic_probe` UDL method was deleted by the wiring
  slice (Rust method, UDL entry, regenerated Kotlin/Swift/Python recipe, and
  Python assertions all removed; the exchange asserts
  `not hasattr(session, 'panic_probe')`): probes must not ship in production
  paths, so no live trap evidence remains. Containment at the generated
  boundary rests on `uniffi_core::ffi::rustcalls::rust_call`
  (`catch_unwind` at the pinned version) — recorded here as a mechanism, not
  a pass.
- Observation from the deleted probe: like all bindings, the panic message
  still reached stderr via the Rust default hook — containment means no
  abort, not silence.

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
- The test-only `panic_probe` was deleted by the wiring slice (absence
  asserted by the exchange).
- `ubm-core` wiring is DONE (see above): the surface calls the core ONLY
  through the `CoreBackend` seam; deeper kernel-transition wiring later
  touches the one `impl`.
- `clippy::large_const_arrays` is allowed crate-wide with justification:
  the lint fires only on the GENERATED metadata const, which cannot be
  fixed in-tree.
