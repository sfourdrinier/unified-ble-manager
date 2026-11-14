# Ordered notification ingress implementation plan

> **For agentic workers:** Use the repository's subagent-driven development workflow. Keep writer ownership disjoint and have the orchestrator integrate and review the shared contract.

**Goal:** Deliver one loss-reporting host-ingress order across selected characteristic instances on every UBM backend, and make it usable by the WHOOP research clients.

**Architecture:** Install an immutable multi-characteristic collection before physical enablement, serialize callback acceptance and terminal controls at a connection-generation owner, and carry the resulting ordered FIFO unchanged to the public consumer. Preserve existing single-characteristic subscription ownership and explicitly distinguish accepted host ingress from earlier radio delivery and durable application acknowledgement.

**Tech stack:** TypeScript public/backend contracts, Rust desktop/mobile cores, Kotlin Android, Swift Apple, native bindings, Electron/Tauri transports and Web Bluetooth.

The [design](../specs/2026-11-07-ordered-notification-ingress.md) defines the acceptance contract. Workstream status below is evidence-based; no physical-device behavior or release is implied.

## Integration in progress

Snapshot 2026-11-08: this work remains an **unreleased integration**. The
published UBM 5.2.1 does not provide this ordered group API. Native, bridge and
public paths below are changing concurrently, so focused results describe the
specific snapshots tested and are not final release qualification.

- A bounded source owner and group FIFO now route source-assigned ordinals,
  membership epochs, explicit loss and terminal evidence. Source sealing,
  retryable cleanup, retained batches and acknowledgement are being integrated
  across desktop and mobile; the final tree still needs review and gates.
- A deterministic public Node/N-API synthetic group run and a public
  Tauri/Rust-dispatcher synthetic run exercised alternating members, retained
  delivery and cleanup. Neither is a physical-radio receipt. Electron's IPC
  group path and Web's callback owner also have focused synthetic tests.
- Android and Apple source/bridge paths have focused native and synthetic
  bridge tests. Android review found bounded admission overflow defects; fixes
  and regression tests are in progress. Their latest integration, native
  artifacts and full public mobile flow require requalification.
- An isolated Linux guest passed the most recent five targeted desktop tests
  on a 13:45 UTC snapshot and earlier BlueZ/private-bus suites on earlier
  snapshots. Central has changed since those runs. Repeat the applicable Linux
  tests on the final source; Windows runtime and device-radio behavior remain
  unverified.
- The WHOOP applications still consume published UBM 5.2.1 and separate
  per-characteristic streams. Durable ordered-group recording, WHOOP migration
  and the proposed native ingress/write marker remain unfinished. No history
  acknowledgement is authorized merely by observing a terminal or a lossless
  host stream. No new WHOOP pairing or device operation was performed.

## Workstream tracker

- [x] Represent host-global source loss separately from exact per-stream overflow in the shared TypeScript stream, React Native owner and IPC/Electron paths; preserve loss and known counters through cleanup. Focused tests and independent review passed; this is not ordered-group support.
- [x] Preserve Web Bluetooth native characteristic object identity across shared leases and retire that cache on link loss, including adapter loss without a browser disconnect event. Share each exact characteristic's physical listener/start across ordinary Web subscribers and stop it on final release. This ownership prerequisite was reviewed; current ordered-group integration still needs final qualification.
- [x] Build and review a bounded Android per-physical-owner FIFO with same-generation fences, explicit loss and fail-stop signaling. The owner is now connected to the radio; final integrated artifacts and public mobile flow still need qualification.
- [x] Build and review an Apple source-owner component with a connection-wide ordinal, bounded group queues, claim-scoped cleanup and explicit platform-tail uncertainty. The standalone harness and synthetic bridge passed on earlier snapshots; final integrated mobile qualification remains outstanding.
- [x] Build and review an additive desktop source event and bounded peer-group owner, including retained route failures and generation-fenced handles. Desktop group routing has synthetic public tests; final integration and actual Windows execution remain outstanding.
- [ ] Define and review `GattDatabase` collection entry point, immutable member identity, stream events, termination/loss semantics, cancellation, physical-subscription sharing, versioned transport and capability declarations in `src/public/gatt.ts`, `src/backend-contract/`, `src/core/` and `src/manager/`.
- [ ] Fix Android's independently identified activation/staging loss and overflow handling in `android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt`, with deterministic interleaving and overflow regression tests in `android/src/test/`.
- [ ] Implement and qualify a connection-owned Android callback ingress and carry group values, generation and controls through `OwnedRadioPort.kt`, `RustRadioHostAdapter.kt`, mobile Rust host/wire and React Native bindings.
- [ ] Implement the same collection at Apple CoreBluetooth callback ingress in `ios/Owned/OwnedCoreBluetoothProtocolRadio.swift`, its classification path, Swift adapter and mobile Rust wire.
- [ ] Replace independently scheduled desktop characteristic forwarders with a single connection-owned source/demultiplexer in `crates/ubm-desktop/src/btleplug_backend.rs`; route group events and terminal controls at `central.rs`. Qualify CoreBluetooth, WinRT and BlueZ, including their actual upstream callback/merge boundaries.
- [ ] Carry one group FIFO and lossless ordinal across Node N-API, Electron main/renderer IPC and Tauri Rust/TypeScript transport. A transport version mismatch fails before changing CCCD state.
- [ ] Implement the equivalent serialized connection ingress in the Web Bluetooth backend without claiming device or radio transmission order.
- [ ] Admit ordered groups to durable recording without replacing original ingress order with per-route pump order; persist identity, ordinal, loss, generation, uncertain tails and recovery outcome.
- [ ] Update WHOOP Bun and Expo capture consumers to use the published UBM ordered collection; retain original bytes, source and loss diagnostics, and withhold history ACK until independent completeness and durable-commit evidence is established.
- [ ] Independently review shared contract and each affected lifecycle path; run focused deterministic tests, repository-required release gates, native/synthetic tests and appropriate physical-radio qualification. Document platform evidence separately and do not advance a release on incomplete required gates.

## Required scenarios

Exercise alternating member callbacks; callback during enable and activation; duplicate service/characteristic UUID instances; old and new generations; independent ordinary subscribers; bounded staging and stream overflow; unlocatable source loss; missing tail; partial enable, cancellation and cleanup failure; database change, disconnect and reconnect; and Electron/Tauri/React Native transport teardown. Verify no accepted event is silently lost, duplicated, reordered or delivered into a later generation. Fault controls cannot be silently dropped or overtaken. For durable groups, additionally test process interruption, replay and recovery before and after application acknowledgement.
