<!-- CGM_DURABLE_INGRESS_BRIEF.md -->

# CGM durable ingress implementation brief

Status: orchestrator-set implementation contract below; no release is authorized by this document. Base: UBM 5.1.3, commit `77ea501813c2f11bb6aeb8c75097d899a1d78a9b`. Heart Studio CGM data acceptance is the trigger: a value accepted by the native subscription but lost before its SQLite archive commit must remain recoverable or have explicit loss evidence.

## Orchestrator implementation decisions

The approved Heart Studio plan explicitly requires extending native-to-JS handoff
where accepted values can be lost. That concrete loss window is confirmed. The
following decisions settle implementation scope; the examples and alternatives
below remain investigation rationale where they differ.

- Add public `GattCharacteristic.subscribeRecorded(options)` alongside ordinary
  `subscribe`, forwarding to the path-based core/backend
  `GattDatabase.subscribeRecorded(path, options)`, with
  `options.recording: { id, maxBytes, maxRecords }` and the
  existing notification/indication selection and cancellation semantics. Return
  an explicit recorded-subscription owner (`subscriptionId`, path, `recordingId`,
  observed delivery when available, and `remove`); no ordinary `values` stream.
- The root public facade exposes `GattCharacteristic.subscribeRecorded(options)` using its existing characteristic navigation; it forwards to the lower-layer `GattDatabase.subscribeRecorded(path, options)`. No new public selector shape is introduced.
- Use the distinct versioned native invoke operation `gatt.subscribe-recorded`.
  Reuse the existing recording registry and journal. Access prepared records
  through the existing `manager.continuation.recordings` controller; do not add
  a second public recording controller/alias merely to rename it. Generalize its
  documentation to describe ordinary recorded subscriptions as well.
- Each recording ID binds one peer and may contain several of its selectors.
  Different peers use independent recording IDs. Open/validate the recording as
  part of recorded subscribe; conflicting quota/owner reuse fails before radio
  effects. Register the consumer before enabling notifications. Journal routing
  is per consumer; do not attach the journal to the shared whole-session outbox.
- The bounded `changed` stream carries payload-free post-commit recording ID
  hints and can report overflow; it is not an acknowledgment. Perform an initial
  prepare/status check, repeat on hints or overflow, and prepare until drained
  plus check status after confirmed remove even if the hint stream already ended. The
  controller must support bounded prepare/ACK after restart without a live radio
  subscription. Never silently fall back to an ordinary volatile stream.
- Reuse native receipt fields already present. Journal and wire parsing preserve
  absent receipt evidence honestly. Retain explicit failure/loss and actual radio
  cleanup semantics. Keep generic recording independent of vendor credentials.
- Ship the capability on supported native paths through the shared Rust owner
  and truthful runtime capability/error handling. Do not introduce a fake
  production backend or duplicate native storage. If a supported route cannot
  carry this contract without a substantive API/ABI decision, stop and report it
  rather than guessing or adding compatibility shims.
- The app will retain Libre's session decryption material with its existing
  Expo SecureStore dependency, explicitly device-bound after-first-unlock on iOS
  and Keystore-backed on Android, account/sensor/recording scoped. Persist it
  before enabling recorded selectors; keep it until authenticated source replay,
  archive commit and journal ACK are complete. This is local reconstruction
  context under the approved preservation scope, not a new backend credential or
  permission to store secrets in a journal, export, log or recorder payload.
- App quotas must derive from actual resource capacity or an explicitly approved
  existing policy; do not invent a clinical/source-retention ceiling. UBM's
  existing explicit quota arguments remain resource admission, never proof that
  unadmitted radio values were preserved.
- Account admission/retirement is explicitly app-owned. UBM remains host-neutral:
  no account IDs, Clerk authority, new credential, or native account-revocation
  API belongs in its journal controller. Heart Studio persists the recording-to-
  account/sensor/session mapping before recorded subscribe and checks its current
  durable account admission/epoch before prepare, archive work and ACK, including
  cold restart. It closes recorded subscription leases on account retirement and
  handles actual cleanup failure truthfully; unread records and replay secrets
  remain under the prior account. A new account never adopts those recording IDs.
  UBM enforces its existing peer/session/generation/lease and recording ownership,
  not application authentication. The account-retirement sentence below refers
  to the app integration, not an unimplemented UBM authorization boundary.

Implementation, tests and documentation may proceed in this isolated branch.
Publishing, consumer catalog cutover and completion require the repository's
normal review/release evidence and final orchestrator integration. No release or
physical qualification is claimed here.

## Public contract

Add an explicit journaled subscription operation on the current characteristic handle, with a separate return type so callers cannot accidentally read both a volatile stream and a journal for the same value:

```ts
const subscription = await characteristic.subscribeRecorded({
  recording: { id, maxBytes, maxRecords },
});
const batch = await manager.continuation.recordings.prepare(id, { maxItems, maxBytes });
// Commit each batch record and its source identity to the app archive.
if (batch.token !== null) await manager.continuation.recordings.acknowledge(id, batch.token);
await subscription.remove();
```

These APIs target the new mobile Rust/React Native capability and are not a claim about an already published release. Recorded subscribe opens or validates the existing journal registry at a host-selected app-private path. The ID is a nonsecret app-selected stable ID with the existing ID/quota bounds. Independent recordings may run for different peers; multiple recorded characteristics of one peer may join one group. A recorded subscription supplies no ordinary value iterator or callback. Its only value path is `prepare`/archive commit/`acknowledge`; ordinary subscriptions and authentication/control sessions retain their existing owners. The bounded `changed` stream gives coalesced post-commit hints and may overflow; the hint is not delivery, ACK, or a second owner. The app performs an initial prepare/status check and repeats after confirmed removal, including when the hint stream ended before the final cleanup tail. The public offline row codec preserves typed bytes and metadata, but its decoded byte count describes the new serialization and it supplies no ACK authority. The app retains the original prepare token until ACK succeeds and can prepare the identical prefix after restart. Backends without the capability report it truthfully; no production memory fallback is permitted.

Each delivered value retains `{recordingId, ordinal}` as durable source identity, peer/session epoch, connection/database generation, immutable canonical selector and delivery mode. Preserve native receipt UTC, monotonic and clock ID when supplied; absence is explicit, never zero or a receipt time inferred at drain. `NotificationReceipt` and optional wire `receipt` already exist in `crates/ubm-core/src/central.rs`, `crates/ubm-mobile/src/host.rs` and `src/backends/reactnative/rust-core-wire.ts`; verify that the installed mobile ABI stamps and persists them. Update the stale claim in `docs/BACKGROUND.md` only after tests establish the actual receipt guarantee. Journal ordinals and metadata already exist in `src/core/continuation-recording.ts`. A notification lost before native journal admission is not a successful archive acceptance; propagate durable ingress-drop/terminal evidence and stop on storage failure or quota exhaustion. No silent memory fallback.

## Native operation and routing

Keep radio ownership with the existing session and one registered physical subscription. Use `gatt.subscribe-recorded` in the versioned session invoke; public callers do not mint operation IDs. The Rust sequence is:

1. Validate host capability, current peer/lease/generation, selector, recording identity/quota, and consumer uniqueness. Open/claim the existing `JournalRegistry` authority; reject conflicting owner or incompatible quota without radio effects.
2. Durably register session, consumer, selector and generation metadata **before** CCCD admission, under the same ordering authority used by notification ingress. A failure before subscribe performs no radio effect. If subscribe fails or cancellation races its completion, preserve/retire registration with a terminal reason and execute the existing unsubscribe/lease cleanup rules; never erase unread records.
3. `MobileHost::drain_route` polls each native value once, stamps or preserves its ingress receipt, and directs the recorded consumer to the journal's atomic `append_batch`. Expose a wake hint only after commit; never enqueue that value into the ordinary `Outbox` data stream. Other consumers, including a separate ordinary consumer on a different selector, retain their current outbox routes. Any explicitly separate subscribers to a physical selector remain separate owners under current UBM semantics; do not create such a tap inside this feature.
4. `prepare` returns an immutable bounded prefix and metadata, including non-value controls and loss, with stable ordinal across retries/restart. The app commits source rows idempotently on `(recordingId, ordinal)` before acknowledging that exact token. Crash after commit and before ACK repeats the prefix without duplicating source rows; crash before commit leaves the prefix unacknowledged.
5. `remove` stops only its subscription and reports actual cleanup; failed cleanup retains the owner for retry. `stop` closes recording admission but is not radio release; `clear` is explicit logical deletion only after stopped and safely archived. Account retirement must fence future reads/ACKs without deleting unacknowledged data. Report loss, storage blocked, unsupported backend, uncertainty and retained cleanup explicitly.

Do **not** attach today's whole-session continuation journal to the shared RN outbox: `crates/ubm-mobile/src/session.rs:608` attaches session-wide before consumer ingress, while `crates/ubm-desktop/src/continuation_outbox.rs:298,612` diverts all durable-session data from the ordinary stream. Route journal admission by recorded consumer/group, reusing `crates/ubm-desktop/src/continuation_journal.rs` and its registry, bounded transaction, prepare and ACK ownership. Do not build a second journal or depend on order between independent callbacks. A session/recording group must support independent peer recordings; don't reuse continuation's one-peer standing order as the ordinary-subscription limit.

## Required Libre replay handoff

Libre `Libre3Session` receives `auth.result.sessionMaterial` in memory and passes its `kEnc` (16 bytes) and `ivEnc` (8 bytes) to `UbmDataPlaneStreamer`; `DataPlaneCrypto` combines IV, packet sequence and kind for authenticated decryption. Session keys are produced from the phase-6 response; the existing account `sensor-state.json` / backup credential owner does not retain this session material. A journal containing only encrypted data-plane fragments cannot be decoded after process death merely by reconnecting: a later session has different material. The existing redundant account files are not demonstrated to be secure storage and must not silently become the secret vault.

Before enabling a Libre journaled data subscription, the app must durably retain its exact session material using its existing Expo SecureStore dependency, scoped to the account, sensor and recording and keyed by a nonsecret immutable journal/session reference and decoder version. Use device-bound after-first-unlock access on iOS and Keystore-backed storage on Android; background and locked-state access remain subject to platform qualification. Never put `kEnc`, `ivEnc`, auth tokens, challenge material or decrypted keys in the plaintext UBM journal, source observation, recorder payload, logs, or default export. Link fragments to their session reference, channel, selector, native ordinal, sequence and receipt; retain the session secret until all relevant fragments have been durably decoded and their source evidence is safely committed. Key retirement must be explicit and cannot precede committed recovery/ACK. Missing or inaccessible session material leaves encrypted records and an explicit blocked state; never ACK them as decoded. This is the settled app-owned admission contract; it is not evidence that the current credential IO already satisfies it.

Replay must preserve fragment ordering and packet assembly boundaries, authenticated plaintext and decoder identity, sensor clock anchor and its uncertainty. Retain unaligned records until an anchor exists rather than inventing a time. A secret vault/session mapping must survive crash between key write and subscription, archive commit and journal ACK, and sensor/account switch. No secret is transferred to the recorder. JS remains the owner of authentication, decryption and vendor semantics: a disconnected sensor requires an OS-admitted JS wake to reauthenticate. This extension does not assert native autonomous CGM reconnect.

## Bounded implementation ownership and gates

- Shared contract and app-facing TS: `src/backend-contract/{operations,gatt}.ts`, `src/core/{core-gatt-handles,subscription-registry,unified-ble-core,continuation-recording}.ts`, RN provider and wire `src/backends/reactnative/{react-native-rust-core-provider,rust-core-wire,react-native-continuation-claim}.ts`; explicit unsupported behavior on nonimplemented hosts. Decide the public API and wire spelling before delegating.
- Rust admission/routing: `crates/ubm-mobile/src/{session,host,wire,continuation}.rs`, `crates/ubm-desktop/src/{continuation_journal,continuation_outbox}.rs`, and `crates/ubm-core/src/central.rs` only where required for receipt/consumer ownership. Swift/Kotlin bridges and `src/NativeUnifiedBleRustCore.ts` change only if existing versioned invoke/control facilities cannot represent the chosen operation; no second native journal. Check both Android and Apple actual paths and version/fail-closed boundaries.
- App integration belongs to Heart Studio: vendor notification consumption, idempotent archive linkage, account-scoped session-secret lifecycle, background wake/replay, explicit pressure/error status. It must read the **published** UBM build, not sibling source.
- Tests: real journal prepare/reopen/ACK across restart before/after archive commit; selector registration before immediate callback; two peers and multiple selectors; same physical selector with independent existing consumers; cancellation during subscribe and retained cleanup; storage full/overflow and missing receipt; original-byte equality and native receipt identity; synthetic Libre encrypted fragmented replay after restart with the correct session secret and explicit inaccessible/wrong-key failure; G7 control response plus glucose ordering and live/history identity. Native synthetic tests do not establish physical collection or power-loss guarantees.
- Complete source/docs/public contract, generated wire/bindings and native artifacts according to `AGENTS.md`, `RELEASE.md`, and `docs/NATIVE_ARTIFACTS.md`. Freeze and qualify a release commit through its required checks, publish through the prescribed release workflow, bump monorepo catalog and install/rebuild both consumer platforms. Final device proof must cover iOS and Android background/locked acquisition, interruption/restart and replay, capacity/failure behavior, and each available Libre/G7/Stelo product. Missing physical devices or unresolved secret access remain explicit required blockers.

## Local native refresh receipt

On 2026-11-03 the canonical `pnpm native:refresh --only android,apple,desktop`
ran from source commit `69c17df53d092c5342b3c2d23a0727675f705bcf`.
Android (three ABIs), Apple (four slices), and macOS arm64 desktop artifacts
passed their producer identity checks. Their respective source digests were
`c2714a19514fc0d9f604348239f6d63e742c0793050975bf3866e7f3dd76bf24`,
`4b22069f0e91cb278ab3d63964e4745664f774887c5fd7203fc95ed2dd8d6976`, and
`a6f5f482856541176b554646395dc96a3b1cbc98d2fb05981af2786906adbf22`.
The Android producer warned that standalone `readelf` was absent and skipped
its separate machine check; NDK `llvm-readelf` verified 16 KiB alignment. This
is local build/identity evidence, not a physical-radio qualification. Other
desktop platform artifacts remain for their respective CI hosts.
