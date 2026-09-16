# LIFETIME_RULES — bindings/jni (UBM 5.0 FFI feasibility)

Tested build: `jni =0.22.4`, `rustc 1.98.1 (48a229cea 2026-09-01)`, Linux
x86_64 cdylib driven from `javac/java 21.0.12` (OpenJDK 64-Bit Server VM)
through real JNI. Contract `C-UBM.0.1.2-DRAFT`, single-owned by `ubm-core`
(workspace member). Proven by `cargo test` and `run_jni_roundtrip.sh`
(51 JVM checks). Anything outside this envelope is a limitation.

## Core wiring (UBM 5.0 wiring slice + U7 transition-driving)

- `CoreBackend` is implemented for the ubm-core-backed `CoreSession`
  (`src/core_backend.rs`, the one implementation in this crate). Revision
  identity, the byte ceiling, and decimal-string counter parsing come from
  `ubm_core::contracts`; no contract constant is duplicated here. The former
  echo-only stand-in (`echo_core.rs`) is deleted — no dual owners.
- U7 transition-driving: `CoreSession` holds a REAL `ubm_core::central::Central`
  (which owns the one scheduling `Kernel`), constructed at `open` for a
  fixed binding-process attachment scope with a completed handshake
  (PKG-02); construction failure fails `open` loudly
  (`lifecycle.invariant-violation|core|echo-session.open|central-construct-failed`).
  Driven natives (all throwing typed `EchoException` on failure, all gated
  on live handles):
  - `nativeCentralStatus` observes the core (frozen revision + live kernel
    counters as JSON; fixed shape across bindings).
  - `nativeExpireSweep(handle, nowMs)` drives a real kernel expiry sweep at
    decimal-string host time (DATA-02 mapping, lossless via
    `BigInteger.toString()`); gate-first ordering keeps post-close
    semantics uniform even for garbage input.
  - `nativeDestroy` drives the real shutdown transition
    (`released` / `release-failed`); idempotent; orthogonal to
    `nativeClose` (echo stays usable until close removes the handle).
  - `nativeBleTransition(handle, transition)` is the loud-rejection path
    for every BLE transition beyond the driven slice:
    `capability.unsupported|capability` (the frozen contract pairing),
    never silent or faked; empty names are `argument.invalid`.
- The echo transport itself stays feasibility-echo (NOT BLE functionality).
  Follow-ups: unique per-instance attachment identity (fixed scope labels
  this slice); surfacing staged kernel effects to a host executor (driven
  batches are bounded and dropped after the call — nothing is staged yet,
  so nothing is lost yet).

## Thread / runtime lifetimes

- Sessions are `Arc<Mutex<CoreSession>>` behind a process-global handle table
  (`long` handles; `0` is never valid). Lookup clones the `Arc` WITHOUT
  holding the table lock during the body, so `close`/`cancel` from another
  Java thread stay effective mid-call (proven: close-during-flight aborts).
- The chunked worker clones the cancel flag under the core lock, then
  releases the lock before the long run. Sync ops hold the core lock only
  for the (short) call.
- JNI local references never escape the call (all conversions are owned).
  No global references are retained: the table owns Rust state, never Java
  objects — so there is no global-ref leak path and no classloader pinning
  from this crate.
- `close()` removes the handle AND destroys the core (arming cancellation):
  in-flight holders abort at the next boundary; stragglers report
  `lifecycle.destroyed` via their own check. Double close rejects loudly.

## Callback invalidation

- No callbacks exist on this boundary (pure call/return + exceptions).
  There is no registration to invalidate — stated as a decision. Event
  delivery for Android consumers (e.g. Wear OS direct-Kotlin paths in the
  FFI-NATIVE card) is a follow-up that must re-prove invalidation.

## Init contract (PKG-02)

- Construction IS the gate: `nativeOpen` with a foreign revision throws
  `protocol.incompatible` and returns `0` (never a usable handle). All other
  natives resolve handles through the table: unknown/closed/`0` handles
  throw `lifecycle.destroyed`. No effect without valid init.

## Cancellation

- Session-scoped armed flag (same semantics as every binding). Armed cancel
  aborts the next chunked unit (entry-take or boundary check) and disarms;
  the session stays usable. Proven from Java threads (worker + `cancel`,
  worker + `close`), strictly asserted with wide timing margins.

## Panic containment

- Every native entry upgrades via `EnvUnowned::with_env` (which contains
  unwinds) and resolves through a custom `ErrorPolicy` mapping panics to
  `lifecycle.invariant-violation`. The former test-only `nativePanicProbe`
  was deleted by the wiring slice (native entry, `EchoBridge` declaration,
  and JVM assertions all removed): probes must not ship in production paths,
  so no live in-VM trap evidence remains — containment rests on the
  `with_env` + `ErrorPolicy` mechanism, recorded here as a mechanism, not a
  pass. (Observation from the deleted probe: the panic message still reached
  stderr via the Rust hook: containment, not silence.)

## Byte ownership

- Entry: `jbyteArray`/`jstring` are COPIED (`convert_byte_array`,
  `try_to_string`) before any core call; null references reject
  `argument.invalid` loudly, never UB. Exit: fresh Java arrays/strings are
  built from owned Rust buffers. Rust never borrows Java memory past the
  call; Java never views Rust memory without a copy. Proven by 512 KiB,
  empty, and null edges in-VM.

## Error identities

- Every rejection throws typed `EchoException` with `code()`/`domain()`/
  `operation()`/`detail()` fields parsed from the shared
  `code|domain|operation|detail` wire message (single-string constructor, so
  typing and wire cannot disagree). The JVM harness asserts FULL wire
  literals, including `detail`.
- Double-fault rule: if the typed throw itself fails, a plain
  `RuntimeException` carrying the wire message is thrown instead — still
  loud, never silent.

## Pinned-dependency findings (jni 0.22.4)

- `Env::throw` is UNUSABLE at this version: its success check is inverted
  (`Throw` returns 0 on success; the wrapper maps 0 to
  `Err(JavaException)`), so every call "fails". `throw_new` reports `Err`
  after SUCCESSFULLY throwing for the same family of reasons. Both were
  proven by observing pending-JVM-exception state, not by reading code
  alone. The bridge therefore keys throw success on OBSERVABLE JVM state
  (`exception_check` after the call) and runs the `RuntimeException`
  fallback ONLY when nothing is pending (running it unconditionally
  clobbered correct typed exceptions during development — observed, then
  fixed). Re-check on any `jni` version bump (follow-up).
- Written in the 0.22 `Env`/`EnvUnowned` idiom (`JNIEnv` is deprecated at
  this version); `unsafe` is confined to `extern "system"` entry signatures
  as the API requires. No raw-pointer code of our own.

## HOST-ANDROID GATT bridge (U5/U9/U10 slice)

- `src/gatt_queue.rs` + `com.ubm.gatt.GattBridge`: Android binder threads
  (GATT/scan callbacks) never drive the core or wait on radio I/O (enqueue
  takes the session mutex, which a worker drain holds for its run, so "never
  block" would overclaim — no I/O waits, only a short in-memory critical
  section). `nativeEnqueueGattEvent` only validates (`argument.invalid` on
  empty/oversize lines, `bytes.too-large` past the wire ceiling,
  `stream.quota` past 1024 queued lines) and stores; a worker thread applies
  the queue FIFO with `nativeDrainGattEvents` against the REAL
  session-owned `Central` (scan/discover/connect/IO/notify/service-change +
  `expire-sweep`, `adapter.reset`, `release`), returning one JSON observation
  object per line. Step-level core rejections are DATA (`ok:false` with the
  frozen contract `code` + `domain`); only the session lifetime throws
  (`lifecycle.destroyed` on unknown/closed handles, including post-close
  enqueue/drain/depth). Known kinds enforce exact positional arity
  (short/over-long lines reject `argument.invalid` as DATA before any core
  transition); unknown kinds fail closed `capability.unsupported|capability`,
  never silently or faked.
- `release` drives the REAL destroy transition (idempotent
  `released`/`release-failed`); like `nativeDestroy` it shuts the kernel
  down, so post-release admission verbs (scan/connect/IO start) fail
  `lifecycle.destroyed` as data. Non-admit verbs differ: `peer.resolve`
  still succeeds, and `adapter.reset` replaces the kernel outright (reviving
  admission while the destroy record stays cached). Native session teardown
  is `nativeClose` (handle invalidation + cancel arming); the Kotlin owner
  calls `release` first, then closes.
- Proven by `cargo test -p ubm5_jni_echo` (GATT queue unit vectors) and the
  `com.ubm.gatt.TestGatt` JVM exchange (44 checks) in
  `run_jni_roundtrip.sh`, plus the 5.0-artifact emulator battery
  (`emulator-probe/five0/`).
- Peer domains are the frozen vocabulary (`public-address`,
  `static-random-address`, ...): Android real-world addresses map to
  `static-random-address`; the harness uses `public-address`.

## Limitations (explicit, not passes)

- Tested on desktop OpenJDK 21 x86_64 (JVM exchange) plus the 5.0-artifact
  emulator battery (`emulator-probe/five0/`, x86_64 AVD, virtual BT adapter
  only — no BLE peers exist there). ART behaviour beyond that battery, other
  ABI splits (arm64-v8a/armeabi-v7a — no physical devices on this lane), and
  the Wear OS direct-call path remain unproven; nothing here implies them.
- The test-only `nativePanicProbe` was deleted by the wiring slice.
- `ubm-core` wiring is DONE (see above): the surface calls the core ONLY
  through the `CoreBackend` seam; deeper kernel-transition wiring later
  touches the one `impl`.
- Supported-ABI list is undecided pending real consumers (FFI-NATIVE card);
  this slice proves the mechanism on one host ABI only.
