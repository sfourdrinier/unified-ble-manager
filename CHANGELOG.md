# Changelog

All notable changes to `unified-ble-manager` are documented here.

## [Unreleased]

No changes yet.

## [4.0.8] - 2026-08-29

### Fixes

- **Apple runtime shutdown no longer retains a closed JavaScript attachment through its invoker.** Closing the native protocol now moves the `CallInvoker` out of shared state before scheduling runtime-thread sink cleanup, breaking the state/invoker/callback ownership cycle. iOS, macOS, and tvOS keep the same public behavior while clean teardown no longer leaves JSI functions alive past their runtime.

- **Public capability checks no longer hide implemented operations whose evidence is limited.** `manager.capabilities.supports(id)` and `require(id)` now agree with the core manager: both `supported` and invocable `limited` descriptors are usable, while `unavailable`, `unsupported`, and missing capabilities still fail closed. Callers that need full qualification can inspect `manager.capabilities.get(id).state` and its retained limitations. This fixes Android callers incorrectly discarding the bonded `when-available` reconnect path merely because its physical-radio evidence is still labelled `limited` (#177, PR #170).

- **Expo Android connected-device monitoring now has an app-controlled notification and explicit lifecycle recovery.** While an active background lease is held, `manager.background.updateNotification({ title, body? })` updates the existing UBM notification in place without acquiring or starting another service. The configured channel, icon, connected-device service type, ongoing state, session-intent policy, and host-app tap are preserved. `restart: 'while-session-intent-exists'` now also manages boot and package-replaced recovery, but only starts the service when native UBM session intent exists; it never scans or reconnects. Once foreground promotion succeeds, a package-scoped `FOREGROUND_READY` signal lets an app-owned headless runtime resume without racing Android's background-service restrictions. Background and restart remain absent/`never` by default, and unsupported hosts reject the operation truthfully (#177, PR #170).

- **React Native no longer closes a healthy notification subscription during a legitimate burst of native events.** Android and Apple still fail closed on an undrained native-to-JavaScript queue, but both bridges now retain up to 512 records / 1 MiB before declaring overflow. That bounded budget accommodates common catch-up transfers such as roughly 288 five-minute readings delivered in one 24-hour history response while preserving a visible `stream.overflow` terminal if JavaScript genuinely cannot keep up. The change applies only to the two React Native native-event ingress queues; other backends keep their existing buffering and capability truth (#175).

- **React Native Android now exposes the system bonded peer directory.** `manager.peers.bonded()` reads the ABI-6 Android bond table, returns deterministic version-1 system-scoped references, and `manager.peers.resolve()` rechecks that a saved reference remains bonded before reconnecting. `manager.connect(peer, { intent: 'when-available' })` reaches Android's queued auto-connect path. Bonded metadata does not imply reachability; Android reports unknown reachability and preserves `permission.denied` instead of returning an empty list. Other backends keep their truthful unsupported boundaries, and Web origin-authorized devices remain distinct from bonded peers.

- **React Native Android now honours `PairOptions.transport: 'le'` instead of discarding it.** The public security layer validated and forwarded the selector, but the React Native Android backend dropped it before the native command and always called parameterless `createBond()`. Explicit LE pairing now crosses native-protocol ABI 5 and invokes Android's transport-selecting bond operation with `BluetoothDevice.TRANSPORT_LE`; `'auto'` remains the platform default. Unsupported or rejected explicit selection fails closed rather than silently changing the request. Because Android API 36 cannot publicly query bond state by transport, only an LE-only device's existing bond satisfies an explicit-LE `already-paired` result; dual-mode, classic, and unknown devices retry the directed operation instead of risking a false success from a BR/EDR-only bond. Reflection-wrapped permission failures retain `permissionDenied`. BlueZ and WinRT already pair through BLE-only device objects, while Apple and Web keep generic pairing unsupported; those platform semantics are unchanged.

- **Every GATT notification was dropped on both React Native backends, and characteristic and descriptor reads with them.** A notification's payload is carried as a binary reference, and the protocol requires that reference to name the operation the event belongs to — the codec compares the two for equality (`requireBinaryCorrelation`). Both bindings stamped the subscribe's correlation on the event while minting the payload under a correlation of their own (`"notification:<subscription>:<ordinal>"` on Android, `"apple-notification:..."` on Apple), a combination that can never validate. Every notification was therefore refused inside our own codec before reaching a caller, so a subscription delivered nothing at all while the radio received the peer perfectly well. The read paths decorated their correlation the same way (`"read:<epoch>:<nonce>"`, `"apple-read:<nonce>"`) and failed the same check on the result record. All four sites now mint the payload under the owning operation's nonce, as the write path always did.

  Sharing one correlation across a subscription's notifications is safe: `OwnedBinaryPayloadStore` keys retained payloads by a freshly generated owner token and releases by that token, treating the correlation as metadata it cross-checks rather than as a unique key.

  The deterministic layer could not catch this, which is why it shipped: the React Native test doubles minted their own notification correlations and omitted the operation correlation entirely, so they modelled a record the native codec refuses to deliver, and the suite stayed green against a shape no device could ever produce. `ReactNativeAndroidProtocolBoundary` now enforces the same rule the native codec enforces, the doubles emit the shape the bindings actually emit, and the codec's own tests pin both the notification event and the read result. Found against a Dexcom G7, where the platform logged the peer's nine reply chunks and the application received none (#168).

## [4.0.7] - 2026-08-26

### Changed behaviour

- `cancelPairing()` reports what the cancellation **achieved**, not what it requested, and can no longer contradict the pairing it cancelled. Every backend answered `'cancelled'` unconditionally the moment the cancel was dispatched, so a cancellation that lost the race told the caller no bond exists while one did — and a caller who believes that never looks again. It now reads the in-flight pairing's own result rather than forming a second opinion, which is what makes the two calls incapable of disagreeing: there is one source of truth and the cancellation reads it. `SecurityCancelPairingResult` gains the words to say so — `'paired'` when the bond completed before the cancellation arrived, and `'rejected'` (carrying the peer's reason) when the peer refused, because claiming credit for stopping something that stopped itself is the same substitution with the arrow reversed. A pairing that *fails* is not given an invented outcome: `cancelPairing()` rejects with the error the pairing rejected with. `'paired'` deliberately matches `SecurityPairResult`'s `'paired'` — a bond exists as a result of this operation — and is not `'already-paired'`, which in that type means the peer was bonded *before* the call; one word, one meaning, in both types. Applies to BlueZ, Android, WinRT **and** the deterministic `/testing` backend, since a mock that answers differently from every real radio lets a consumer's suite pass against a contract no device honours. WinRT additionally short-circuited *upstream* of the shared mapper — when its dispatcher reported the operation already terminal, which is exactly the lost race, it answered `'not-pairing'`, contradicting its own `pair()` and making that word mean both "there was nothing to stop" and "it was already over" (#159).

### Additions

- **`PairOptions.secureConnections` can now actually select the LE pairing generation on BlueZ — behind a host-supplied privileged operation.** Some peripherals accept only LE Legacy and terminate the link on a Secure Connections pairing request; 4.0.6 gave the contract a way to say so but no backend that could honour it. `org.bluez.Adapter1` (BlueZ 5.85) exposes no Secure Connections property and `Device1.Pair` takes no parameters — the setting lives behind the kernel management socket's Set Secure Connections command, which requires **`CAP_NET_ADMIN`**.

  This package never acquires that privilege: it opens no management socket, shells out to nothing, and does not assume it is root. A library that silently escalates hands an application capabilities its author did not choose and cannot audit. The **host** supplies the operation via `BluezBackendProviderOptions.pairingGeneration`, which makes the escalation visible in the application that opted into it. Omit it and `'require'`/`'disallow'` keep failing closed exactly as in 4.0.6 — the default posture is unchanged.

  Three properties of the kernel setting a host must accept, documented in `docs/BONDING.md` rather than discovered: it is **adapter-wide**, not per-pairing, so every pairing on that controller uses the selected generation while it is held; it **outlives the process**, because the kernel keeps it until something sets it back; and a **failed restore never changes the pairing's outcome** — a bond that was created is reported as created, with the restore failure reported separately, because leaving a controller in LE Legacy and telling a caller they are not bonded are both serious and are different facts. Concurrent *directed* pairings on one adapter are serialised so they cannot corrupt each other's restore value; an undirected (`'prefer'`) pairing does not take that lock and uses whichever generation is held, which `docs/BONDING.md` states beside the option.

  `security:pairing-generation` joins the capability catalog and is reported by the instantiated backend at runtime: `unsupported` with the platform reason when no operation was supplied, `limited` when one was — so two BlueZ backends on one machine legitimately answer differently. Evidence is deterministic tests only; this is **not** physical-radio proof, and the label says so (#144).

### Fixes

- A Tauri connect that succeeds physically but is then denied admission no longer strands the peer. Both compensation branches — the caller vanished, and the caller's lease went stale — removed the peer reservation *before* attempting the disconnect and then discarded its result with `.ok()`. If that disconnect failed or hung while the peripheral stayed connected, the result was a connected peer with no owner and no handle: nothing could reach it, nothing could retry it, and the caller was told only that admission was denied. Compensation now follows the rule the explicit disconnect already follows — a bounded wait, a fresh state reading, and a D-Bus error naming a vanished device object read as evidence of release rather than as a failed question — and surrenders the reservation only when the platform proves the link is down. Genuine indeterminacy keeps it so a retry can reclaim the peer, and the admission error carries what compensation could not undo, which is the difference between "try again" and "a peer is stranded" (#146).

### Known limitations

- `pair()`'s abort path still reports `'cancelled'` without knowing whether the daemon bonded anyway. Learning the truth means waiting for the radio, and waiting risks a hang that a wedged daemon would inflict on the caller — which the suite explicitly forbids. Resolving it needs a vocabulary that can express "cancellation requested, bond state not yet known"; `state()` and `watch()` remain authoritative meanwhile (#157).

## [4.0.6] - 2026-08-26

The first release cut against a live peripheral on Android as well as Linux. Most of it is defects that only real hardware surfaced, several of them boundaries that discarded evidence and so presented a specific fault as silence. Does not retag `v4.0.5`.

### Fixes

- A Tauri disconnect that fails no longer leaves the peer owned when the platform can prove it was released. btleplug's CoreBluetooth `disconnect()` never resolves once the peripheral has left its internal map, so the call is bounded and the outcome decided by a fresh state reading: gone means released, still connected means the retry keeps ownership. On BlueZ the state reading is classified before it is stringified, because a removed D-Bus device object is not a failed question but the answer to it - previously that case was treated as unknowable and the peer stayed owned until the process restarted. Genuine indeterminacy - a D-Bus timeout, the daemon gone, the adapter pulled - still retains ownership (#145, #154).

- BlueZ no longer reports a pairing it could not stop as `cancelled`. `Device1.CancelPairing` rejections were all swallowed, so a refusal from bluetoothd — `org.bluez.Error.Failed`, `NotAuthorized`, a D-Bus timeout, the daemon gone — left the in-flight `Pair` running while `cancelPairing()` returned `cancelled` and the abort/deadline path did the same. A caller told no bond exists while one is still being made cannot recover, because it never learns to look. Only the two rejections that prove nothing is left running are treated as success — `org.bluez.Error.DoesNotExist` and `org.freedesktop.DBus.Error.UnknownObject`, an answer to the question rather than a failure to answer it, the same pair the Tauri disconnect path classifies for the same reason. Anything else now surfaces as `platform.failure` carrying the D-Bus error name (#143).

- `scan()` works on React Native Android again, and so does every other control-plane operation there. The Android dispatcher stamped a hardcoded `1` on field 1 of every result and event it built while the native codec has required version 2 since the schema was frozen, so all of them — `scanStarted`, a connect completing, a connect failing, a cancellation, a bond state change, a lost link — were rejected at the JSI boundary as version-incompatible and dropped. A scan therefore never received its terminal: the radio discovered the peripheral 38 times in one four-minute capture and the application was handed nothing. The stamp now comes from the generated `NATIVE_PROTOCOL_VERSION`, so it cannot drift from the version the codec validates against (#140).
- A record the codec refuses is no longer only written to logcat: the Android binding counts quarantined records per attachment and emits a `recordQuarantined` diagnostic in place of the record it could not deliver, so an application learns that the boundary discarded something it was asked to deliver (#140).
- A version-incompatible record names its kind, the version it carried and the version expected, so a quarantined record identifies its emitter instead of leaving four record kinds to choose between (#140).
- `adapter.waitUntilReady()` no longer reports a radio that is still starting up as one the caller is not permitted to use. The React Native Android boundary answers `authorization: 'unavailable'` until the radio publishes its first authoritative state, and the readiness gate maps that value to `permission.denied` — so the one API whose purpose is to wait that window out failed on the very state it exists to wait for, on a phone with Bluetooth switched on and all four runtime permissions granted. The pending snapshot now reports `authorization: 'unknown'`, the value its `availability` and `power` siblings already carried for the same reason: the absence of a measurement is never a denial, so no readiness gate may block on it (#156).
- BlueZ states its connection-control truth instead of omitting the concept: `connection:priority` and `connection:parameters` are now registered as explicitly `unsupported`, with limitations naming the BlueZ D-Bus gap (no LE connection-parameter API on `org.bluez.Device1`/`Adapter1`, BlueZ 5.85), the privilege requirement of the alternatives (`CAP_NET_ADMIN` kernel mgmt socket and root-only debugfs — neither a live per-connection update), and the consequence that GATT traffic may run at a multi-hundred-millisecond peer-negotiated interval. `requestPriority()`, `parameters()`, and `parameterEvents()` fail closed with that reason attached (`BleError.limitations` and `platform.safeMessage`) instead of a bare `capability.unsupported` that taught nothing while a slow link looked like a peripheral fault (#149).
- BlueZ `security.pair()` no longer fires `Device1.Pair` when the operation is aborted or times out while the just-works agent is still registering; it re-checks cancellation after agent registration so a cancelled pairing never proceeds on the daemon (#143).
- BlueZ `security.pair()` reports `paired` (not `cancelled`) when an abort or deadline lands after `Device1.Pair` has already completed the bond, so a bond that was actually created is never reported as if it never happened (#143).
- Android `security.pair()` no longer reports a timed-out pairing as `cancelled` when native cancellation is unavailable and it cannot actually stop the in-flight bond; the deadline path now fails closed with `capability.unsupported`, matching the abort path, so a bond that may still complete is never misreported as cancelled (#143).

### Additions

- `FindOptions` accepts `duplicates` and `delivery`, so the `find()` convenience can be pointed at a peripheral that advertises in dense bursts instead of being abandoned for a hand-driven `scan()`. Both default to the values `find()` has always used (`'coalesced'` and `'latest'`), so nothing changes for existing callers (#148).
- BlueZ pairing dispatch: `org.bluez.Device1.Pair`, `org.bluez.Device1.CancelPairing`, and `org.bluez.Adapter1.RemoveDevice` are now allowed through the dbus-next boundary, and the backend registers a just-works (`NoInputNoOutput`) `org.bluez.Agent1` on its own bus (not the system default) so a client-initiated pairing can complete without an external agent (#141, #143).

### Changes

- The Node convenience factories (`createBluezBleManager` and siblings) now select the first adapter (ordered deterministically by id) when the caller names no `adapterId`, instead of failing on a multi-adapter host. A single-adapter machine needs no configuration and a multi-adapter host picks the same controller every run; pass `adapterId` to target a specific controller (e.g. a second USB dongle used for debugging). The low-level provider is unchanged: it still requires an explicit selection and never silently substitutes one (#143).
- `PairOptions.secureConnections` (`'require' | 'prefer' | 'disallow'`, default `'prefer'`): request an LE pairing generation. `'prefer'` defers to the platform. No current backend exposes per-pairing generation selection, so `'require'` and `'disallow'` fail closed with `capability.unsupported` on BlueZ, WinRT, Android **and the deterministic `/testing` backend** rather than being silently ignored; the contract is in place for a backend that can honour it. The deterministic backend previously ignored the field and reported a Secure Connections bond under the exact option that forbids one, so a consumer's suite could pass against a contract every real radio rejects — the one way test infrastructure can actively mislead. A test that asserted a successful pair under `'require'` or `'disallow'` now sees `capability.unsupported`, which is what production would have done (#144, #143).

### Documentation

- Every hardcoded timing and capacity constant in `src/` is now classified as a protocol invariant, a safety bound, or host policy, and each one that stays fixed carries a comment at its definition saying why. Undocumented constants were the defect: a fixed deadline that is never explained reads as a device fault when a slower host misses it (#148).
- `find()`'s 10 s fallback deadline and `adapter.waitUntilReady()`'s 10 s fallback deadline are named constants shared between the in-process and IPC managers, so the same logical operation cannot expire at two different times either side of the IPC boundary (#148).

## [4.0.5] - 2026-08-25

First release cut against a live peripheral. Every fix below came from driving a real CGM from an Android phone and a Linux/BlueZ host, and several are defects no unit test had reason to catch. Does not retag `v4.0.4`.

### Fixes

- `readiness()` no longer reports a working radio as `unavailable` before the Android adapter publishes its authoritative state; a boundary that has not yet been told the radio's state is a pending condition, not an absent radio (#116, #128).
- The direct-GATT backend is shared by CoreBluetooth and React Native Android, so its diagnostics now name the platform that is actually running — `[unified-ble:android-gatt.*]` rather than `[CoreBluetoothBackend.*]` — and its operation ids are `direct-gatt.*` instead of `corebluetooth.*`. Scan-stop cleanup timeouts carry the platform identity instead of `platform: null` (#117, #128, #132).
- An address known out of band can now enter the system: `ScanClause.addresses` and `connect({ address })`, gated on the reported `peer:address-targeting` capability, with pending BlueZ semantics that complete whenever the peripheral next advertises, and Android `ScanFilter.setDeviceAddress`. Hosts that cannot express a radio address — CoreBluetooth, Web, and the IPC transport — report the capability unsupported and fail closed rather than advertising something they cannot honour (#118, #128, #131, #133).
- `ScanOptions.platform` is honoured instead of unconditionally rejected, so consumers can select Android scan mode; peripherals that advertise infrequently are no longer effectively undiscoverable behind the platform default duty cycle. Gated on `scan:platform-options` in the public, core and IPC paths; `match-lost` and pre-26 `legacy: false` fail closed rather than silently doing something else (#120, #130).
- BlueZ GATT works again: occurrences are decimal indices rather than D-Bus object paths, so `discover()` no longer fails with `protocol.violation: public-gatt.occurrence` on every device — a regression introduced when the public validator was tightened without migrating the backend. Two same-UUID services are now distinctly addressable, which neither the old leniency nor the strict check managed (#123, #129).
- The BlueZ boundary decodes the `y` and `a{qv}` D-Bus variants that BlueZ 5.85 actually sends, and routes `Adapter1.ConnectDevice` through the boundary instead of rejecting it as locally unsupported; without these the backend could not attach at all (#128).
- Public BLE resources are portable across copies, and closing an adapter watch's value stream tears the watch down instead of leaving a 25 ms poll timer and an abort listener alive (#122).
- `@babel/runtime` is declared as a production dependency. The emitted CommonJS imports its helpers, so an external linked checkout failed with `MODULE_NOT_FOUND` while repository-local tests passed against a transitive copy — a packaging gap only a real consumer could surface (#134, #135).
- A database stream is finalized after an invalidation retry succeeds, and per-subscription React cleanup and failed IPC CCCDs stay owned (#127).

### Compatibility

- Native protocol ABI 3 → 4. A JavaScript bundle carrying the new scan fields now fails attachment negotiation against an older native binary instead of failing at scan-start — or, worse, silently selecting the opposite scan mode.

### Release integrity

- Cut from the exact `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels. The live-hardware runs behind these fixes are development evidence, not qualification evidence.

## [4.0.4] - 2026-08-25

Post-4.0.3 audit: wire/scan/IPC ownership, Android 16 KB ELF alignment, Apple teardown, abortable Web chooser honesty, React remount-owned cleanup, and React Native entropy without WebCrypto. Does not retag `v4.0.3`.

### Fixes

- Tauri wire codec budgets, public scan presence overflow, IPC contract errors, GATT admission/rediscovery, Android scanCallback/receiver commit-after-success, 16 KB native page size (#80, #81, #83, #84, #93, #85, #86, #100, #107).
- Apple disconnect confirmation, Service Changed CCCD retry, restored notify-off; Web chooser abort/timeout does not retain a late `requestDevice` grant (#87, #89, #90, #88).
- React adapter-watch fail-visible terminals, remount-owned scan/characteristic `release-failed`, scoped BleProvider barriers, composeAbortSignal listener cleanup, `managerKey` replacement (#96, #97, #98, #99, #101).
- React Native manager construction no longer depends on WebCrypto or a Metro-fatal `crypto` require; native CSPRNG is the default, with injectable `randomBytes` (#111, #113, #114).

### Release integrity

- Cut from the exact post-PR #110 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.3] - 2026-08-25

Lifecycle ownership is explicit through clone/decode, public stream close, IPC admission, Web/Tauri teardown, backend unregister/overflow native release, and React hook store/terminal cleanup. Does not retag `v4.0.2`.

### Fixes

- Serializable clone/decode stays fail-closed; public stream close, scan stop, and IPC/Web/Tauri teardown no longer drop `release-failed` or skip native disconnect (#58, #59, #75, #60, #72, #63, #73, #76, #74, #79, #67, #77, #78).
- Adapter, security, and event-stream close/overflow keep backend ownership until native release succeeds; overflowing CoreBluetooth scan owners do not stop remaining joiners (#61, #68, #69, #70, #71).
- React adapter-state watches cannot wedge or double-own a run; `useDiscoveredPeers` is bounded and honors lost-peer events; connection and characteristic hooks leave loading with a fail-visible terminal (#62, #65, #66).

### Release integrity

- Cut from the exact post-PR #105 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.2] - 2026-08-24

Public coalesced scans no longer lose duplicate suppression after lost-peer or presence-cap churn: fingerprint deletions now decrement the retained-byte counter. IPC pre-registration buffering is globally bounded (ID/item/byte/age) with fail-visible tombstones. Connection release attempts `connection.disconnect` even when lifecycle unsubscribe rejects or returns `release-failed`, and preserves both cleanup failures. Does not retag `v4.0.1`.

### Fixes

- Coalesced public scans keep exact fingerprint byte accounting across `reportLost` and presence eviction (#53).
- IPC pending streams for unknown IDs are aggregate-bounded; quota/TTL loss is visible when the ID later registers (#54).
- IPC connection release no longer skips physical `connection.disconnect` after a lifecycle unsubscribe failure (#56).

### Release integrity

- Cut from the exact post-PR #57 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.1] - 2026-08-24

Tauri JS transport now accepts the adapter-state snapshot the Rust plugin emits (`heard: null` on unsampled bootstrap, non-negative integer on live `adapter.state`). Extra or missing adapter-state keys stay fail-closed and are named in the decode error. Teaching docs describe the current stable 4.x install instead of RC-as-current. Does not retag `v4.0.0`.

### Fixes

- `createTauriBleManager()` no longer throws `protocol.malformed: tauri.transport.response` because Rust included `heard` on `adapter.state` (#50).

### Release integrity

- Cut from the exact post-PR #52 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as `latest`; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0] - 2026-08-24

Stable package/API contract after PR11/RC5. Package SemVer `4.0.0` does not promote backend support labels or claim physical-radio evidence. Live-radio vertical slices, soak, and crates.io crate publication remain unverifiable in this environment and are recorded as blocked rather than mocked. Immutable `v4.0.0-rc.5` is not retagged.

### Qualification

- Deterministic, package, plugin, lint, and generated-artifact gates pass on the exact `main` candidate.
- Generated platform-support evidence stays Experimental / not bound to this artifact; compile/ABI/deterministic proof is not relabeled as live-radio.
- Tauri documented install remains crates.io (`tauri-plugin-unified-ble-manager@4.0.0`) with checkout `path` fallback until the crate is published.

### Release integrity

- Cut from the exact post-PR12 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as the stable `latest` package; this does not promote backend support labels.

## [4.0.0-rc.5] - 2026-08-24

Distribution and consumer-tooling freeze after PR11. Documented Tauri install is crates.io (`tauri-plugin-unified-ble-manager@4.0.0`); the crate is not yet published and `path` remains the checkout fallback until it is. Public CLI taxonomy is `doctor`/`inspect`/`init`/`support-bundle` plus `backend tck`/`backend scenario` routing. `TAURI_PLUGIN_COMPATIBILITY` and `createTestBleEnvironment` are exported from `/tauri` and `/testing`. This does not mint live-radio evidence or change RC2 portable BLE semantics or the RC4 Expo schema. Immutable `v4.0.0-rc.4.1` is not retagged.

### Packaging and CLI

- `ubm doctor` without `--backend` reports package/runtime identity and labels `proofBoundary: compile-config-loadability`.
- `ubm init --host tauri` writes a crates.io Cargo fragment; other hosts write stable public-API factory fragments and refuse overwrite without `--force`.
- `ubm inspect config|capabilities --host` and `ubm support-bundle create` do not load a radio or upload data.

### Release integrity

- Cut from the exact post-PR #48 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.4.1] - 2026-08-23

Fail-closed and IPC/governance closure from the RC3 audit on the post-PR10 `4.0.0-rc.4` line. PR11 / `4.0.0-rc.5` remains paused. Backend support labels remain evidence-derived and are not promoted by this release. Immutable `v4.0.0-rc.4` is not retagged.

### Public API and semantics

- Signature-aware API report checking; GATT included-service and property schema validation; connection-control echo validation; supervisor `stop()` no longer returns `released` while late configure owns the session; `BleCleanupError` retains the exact cleanup record; IPC local stream overflow tears down the remote producer; invalid Electron `deliveryMode` is rejected; Node host factories rehydrate to `BleError`.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics.

### Release integrity

- Cut from the exact post-PR #46 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.
- Repository: `main` is protected by ruleset `protect-main`; stale `agent/*` remotes were archived/deleted.

## [4.0.0-rc.4] - 2026-08-23

Expo host and native/residual scan-planning release candidate after PR10. This candidate includes the Expo config-plugin v2 schema, thin Expo factory and React lifecycle integration, optional Android foreground-service and companion association surfaces, iOS restoration identity, and the PR9 native/residual scan planner. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and host integration

- Completed the additive PR9–PR10 host surfaces while preserving the RC2 manager, GATT, error, capability, lifecycle, and scan-query contracts.
- Froze the Expo config-plugin and restoration configuration schema at this candidate. `createExpoBleManager()` remains a thin composition over the React Native factory/native provider.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics across native and host boundaries.

### Release integrity

- Cut from the exact post-PR10 `main` merge commit through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.3] - 2026-08-22

Advanced central release candidate after PR8. This candidate includes known-peer directories, connection intents and reconnect supervision, pairing/security semantics, advanced link controls, write readiness, and bounded GATT recovery. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and semantics

- Completed the additive PR5–PR8 central features while preserving the RC2 manager, GATT, error, capability, lifecycle, and scan-query contracts.
- Added explicit link-control observations and requests, connection-intent supervision, pairing/security result vocabulary, and safe service-change/cache-recovery behavior.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics across native and host boundaries.

### Release integrity

- Cut from the exact RC3 release commit `a423a73` on `main` through the tag-driven trusted-publishing workflow.
- Intended for publication as a prerelease candidate on the `latest` channel; this does not promote backend support labels or claim physical-radio evidence.

## [4.0.0-rc.2] - 2026-08-21

Catch-up release candidate for the frozen 4.0 portable runtime contract. This candidate is cut from the exact post-PR5 `main` commit because the planned RC2 publication was delayed; it includes the PR1–PR5 contract and peer-directory work. Backend support labels remain evidence-derived and are not promoted by this release.

### Public API and semantics

- Completed the application-only manager, capability/recovery, GATT object, canonical scan-query, and scoped peer-reference surfaces.
- Added truthful backend peer-directory capability wiring, origin-authorized Web Bluetooth resolution, and fail-closed unsupported behavior for hosts without a proven directory boundary.
- Preserved bytes-first payloads, `AbortSignal` cancellation, generation-bound resources, and explicit ownership/cleanup semantics.

### Release integrity

- Intended for publication as a prerelease candidate on the `latest` channel through the tag-driven trusted-publishing workflow.
- This is a release-train catch-up point; RC3 remains the post-PR8 checkpoint for the advanced central feature set.

## [4.0.0-rc.1] - 2026-08-19

Documentation correctness and pre-stable API fixes on the 4.0 contract. This does not promote backend support labels or claim live-radio evidence.

### Public API

- Removed invalid SIG read helpers `readHeartRateMeasurement`, `readBloodPressureMeasurement`, and `readTemperatureMeasurement`.
- Profile commands now reject missing characteristic properties with `gatt.property-not-supported` before calling the backend.
- Renamed Expo plugin option `isBackgroundEnabled` to `requiresBluetoothLeHardware` and rejected iOS `peripheral` background mode.
- Added `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG` (legacy `BLEPLX_PLUGIN_DEBUG` still enables plugin debug).
- Added application factories `createReactNativeBleManager({ clientId, managerId, hostSessionScope })`, `createNavigatorWebBleManager` default environment, and `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`.
- Added `BleManager.adapterStates()`, `defaultScanDelivery()`, `scanForServices()`, `withDiscoveredConnection()`, and `throwIfCleanupFailed()`.
- Renamed the injectable RN factory to `createReactNativeBleManagerWithEnvironment`.
- Default Web `visibilitychange` handling now reports `page-hidden` only when the document is hidden.
- `adapterStates({ signal })` re-checks abort after `watchState()` and closes the watch instead of leaking it.

### Documentation and examples

- Finite helper-first README Heart Rate journey, Expo/bare setup paths, and migration fixes for cancellation, coexistence, scan merge policy, and shared deadlines.
- Example BLE service lifecycle, overflow, path resolution, and stable client identity.

## [4.0.0-rc.0] - 2026-08-17

First publication from `sfourdrinier/unified-ble-manager`. This is the 4.0 package/API contract as a release candidate on npm `latest`, so `pnpm add unified-ble-manager` installs this build. It does not promote backend support labels or represent the stable 4.0.0 release.

### Stable package and public contract

- Established the Unified BLE Manager 4.0 package/API contract as its first release-candidate baseline.
- Established `sfourdrinier/unified-ble-manager` and `main` as the canonical repository and release branch.
- Preserved the Git ancestry of the 4.0 work while leaving `sfourdrinier/react-native-ble-plx` as the historical and 3.x home.
- Kept platform support qualification independent from package SemVer: stable `4.0.0` does not promote a backend beyond the support label justified by retained evidence.

### Package and host model

- Finalized the host-neutral root plus explicit React Native, Web, Electron, Node/CoreBluetooth, Node/WinRT, Node/BlueZ, backend SDK, testing, codecs, CLI, and profile exports.
- Standardized public BLE data on `Uint8Array`, cancellation on `AbortSignal`, explicit manager ownership, bounded event semantics, typed capabilities, and versioned backend/native protocols.
- Kept React Native, browser, Electron, Node, and third-party backend integration explicit; no production path silently falls back to Noble, Web Bluetooth, or a simulated backend.

### Release integrity

- Migrated canonical CI and release automation from the legacy `master`/`4.0` topology to `main`.
- Initial stable publication requires the release tag to identify the exact current `main` commit and reruns package, native-build, ABI, artifact, packed-consumer, and supply-chain checks before npm publication; post-publish recovery reuses the immutable npm tarball.
- Canonicalized package repository, issue, homepage, podspec, SBOM, and release metadata to the new repository.
- Canonicalized project licensing to Apache-2.0 and regenerated the SBOM and third-party license inventory from the final release metadata.
- Retained evidence-based platform labels without inventing physical-radio proof that has not been captured.

### Migration

- Reworked the README, migration guide, release guide, support/security guidance, roadmap/evidence documentation, and GitHub issue intake for the standalone multi-host project.
- `v4.0.0-alpha.40` remains the historical repository-migration checkpoint and final published alpha before stable 4.0.0.

## [4.0.0-alpha.40] - 2026-08-02 (published prerelease)

### Added

- Added a versioned Electron renderer API for connection lifecycle subscriptions, including client-generated stream admission, connection and renderer ownership isolation, overflow reporting, terminal delivery, and explicit unsubscribe cleanup.
- Added deterministic coverage for link loss while the renderer is otherwise idle, partial aggregate cleanup, cancellation and late completion, stale generations, renderer destruction, bounded cancellation ledgers, and retryable remote detach ownership.

### Fixed

- Prevented connection events from pumping before renderer admission and prevented partially failed renderer destruction from leaving a local subscription active after main-process ownership was already detached.
- Made synthetic cleanup terminals deterministic and zero-counted without changing ordinary overflow accounting, while preserving idempotent cleanup retry and prohibiting duplicate native detach.

### Support and evidence boundary

- Alpha.40 adds deterministic Electron lifecycle transport and package proof; it does not add a physical-radio evidence record or promote any backend support label.
- Alpha.40 remains Experimental. Meta Quest and the controllable physical fault-injection peripheral remain deferred to 4.1.

## [4.0.0-alpha.39] - 2026-08-01 (published prerelease)

- Previous 4.0 prerelease. See the preserved detailed history for the complete alpha train.

## Earlier history

The complete detailed pre-stable changelog is preserved byte-for-byte in [`CHANGELOG_HISTORY.md`](https://github.com/sfourdrinier/unified-ble-manager/blob/main/CHANGELOG_HISTORY.md), in addition to the full Git ancestry. It contains the alpha train and inherited project release notes without forcing the new canonical changelog to carry every historical entry inline.
