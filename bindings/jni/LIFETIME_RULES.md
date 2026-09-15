# LIFETIME_RULES — bindings/jni (UBM 5.0 FFI feasibility)

Tested build: `jni =0.22.4`, `rustc 1.98.1 (48a229cea 2026-09-01)`, Linux
x86_64 cdylib driven from `javac/java 21.0.12` (OpenJDK 64-Bit Server VM)
through real JNI. Proven by `cargo test` and `run_jni_roundtrip.sh`
(35 JVM checks). Anything outside this envelope is a limitation.

## Thread / runtime lifetimes

- Sessions are `Arc<Mutex<EchoCore>>` behind a process-global handle table
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

## Panic containment (proven in-VM)

- Every native entry upgrades via `EnvUnowned::with_env` (which contains
  unwinds) and resolves through a custom `ErrorPolicy` mapping panics to
  `lifecycle.invariant-violation`. `nativePanicProbe` proves the VM SURVIVES
  a Rust panic, Java receives the TYPED `EchoException`, and the session
  stays usable. (The panic message still reaches stderr via the Rust hook:
  containment, not silence.)

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

## Limitations (explicit, not passes)

- Tested on desktop OpenJDK 21 x86_64 only. Android ART behaviour, ABI
  splits, and the Wear OS direct-call path are unproven (FFI-NATIVE
  follow-up on real Android tooling); nothing here implies ART acceptance.
- `nativePanicProbe` is test-only and must be deleted before any
  production-shaped use.
- No `ubm-core` wiring yet: the surface calls the core ONLY through the
  `CoreBackend` seam (explicit follow-up).
- Supported-ABI list is undecided pending real consumers (FFI-NATIVE card);
  this slice proves the mechanism on one host ABI only.
