# Ordered notification ingress implementation plan

> **For agentic workers:** Use the repository's subagent-driven development workflow. Keep writer ownership disjoint and have the orchestrator integrate and review the shared contract.

**Goal:** Deliver one loss-reporting host-ingress order across selected characteristic instances on every UBM backend, and make it usable by the WHOOP research clients.

**Architecture:** Install an immutable multi-characteristic collection before physical enablement, serialize callback acceptance and terminal controls at a connection-generation owner, and carry the resulting ordered FIFO unchanged to the public consumer. Preserve existing single-characteristic subscription ownership and explicitly distinguish accepted host ingress from earlier radio delivery and durable application acknowledgement.

**Tech stack:** TypeScript public/backend contracts, Rust desktop/mobile cores, Kotlin Android, Swift Apple, native bindings, Electron/Tauri transports and Web Bluetooth.

The [design](../specs/2026-11-07-ordered-notification-ingress.md) defines the acceptance contract. Workstream status below is evidence-based; no physical-device behavior or release is implied.

## Workstream tracker

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
