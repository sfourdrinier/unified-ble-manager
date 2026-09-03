<!-- docs/GAPS.4.0.md -->

# Unified BLE 4.0 platform, CI, and evidence inventory

**Status:** Current implementation and evidence inventory; not architecture authority

**Architecture authority:** [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

**Product scope:** [`../ROADMAP.4.0.md`](../ROADMAP.4.0.md)

## How to read this inventory

This file tracks platform code, CI, package, lab, and live-radio evidence. It does not define the public API, backend contract, compatibility policy, host selection, or implementation sequence. Those decisions belong to the implementation plan and accepted ADRs.

The clean-baseline contract, unified core, public manager, deterministic backend, TCK, native protocol, first-party backend implementations, host-isolated package exports, SDK/CLI, and legacy-absence gates exist in the 4.0 source. Passing deterministic, compile, ABI, or package tests are implementation proof; they do not become physical-radio support evidence unless a retained record proves the corresponding live scenario.

This source targets `unified-ble-manager@4.0.16`; the npm registry and release
provenance, not a source version string, determine whether it is published.
Earlier stable releases remain immutable published history. Backend support labels remain
evidence-derived. This package does not rewrite the evidence inventory: a backend remains at the support level
justified by its records. The alpha train, including `v4.0.0-alpha.40`, remains historical implementation/release
evidence and must not be used to claim a higher current support label than it actually proved.

## Proof levels

| Level | Meaning                                                   |
| ----- | --------------------------------------------------------- |
| L0    | Reviewed design, inventory, or evidence record            |
| L1    | Unit, contract, or deterministic scenario proof           |
| L2    | Compile, link, package, or artifact proof                 |
| L3    | Real-OS smoke without the required live-radio scenario    |
| L4    | Declared live-radio vertical slice                        |
| L5    | Background, restart, reconnect, or soak reliability proof |

A label may claim only the evidence it has. Deterministic injection, mocks, a system probe, or compilation cannot satisfy an L4/L5 claim. Missing hardware is an explicit blocked evidence state, never a waiver.

Deterministic fault injection must never be presented as live-radio proof.

## Current implementation and evidence matrix

| Backend or environment                           | Implementation/package state                                                                                                                                                     | Minimum proof for the claimed support label                                                                                         | Remaining evidence work                                                                                                      |
| ------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Deterministic test backend                       | Implemented contract/core/TCK path with virtual time, programmable peripheral behavior, fault injection, scenarios, and zero-resource cleanup assertions                         | Full TCK, virtual-time scenarios, deterministic fault injection, package binding where claimed                                      | Retain current package-bound L1/L2 records for each release where required                                                   |
| React Native Android                             | JSI binary protocol, owned Android radio, descriptors, cancellation, generations, restoration limitation, TCK registration, Android/Expo compile lanes                           | Native protocol, TCK, package/compile, plus live/background evidence required by the public label                                   | Physical Android vertical slice, lifecycle/background/Doze and declared OEM-matrix evidence                                  |
| React Native Apple                               | JSI binary protocol, owned CoreBluetooth radio, descriptors, bounded pre-JS ingress, cancellation cleanup, restoration adoption, TCK registration, iOS/tvOS compile lanes        | Native protocol, TCK, package/compile, plus live/restoration evidence required by the public label                                  | Physical iPhone/iPad vertical slice plus restoration/background evidence on declared systems                                 |
| Web Bluetooth                                    | Chooser-specific backend, authorization semantics, notifications, lifecycle hardening, browser-safe bundle, TCK, public scenarios, physical-validation harness                   | Browser build, TCK, and the live Chromium proof required by the public label                                                        | Retained physical chooser/connect/discover/read/notify/cleanup evidence on declared browser/OS combinations                  |
| BlueZ                                            | Owned ObjectManager/D-Bus backend, adapter/scan/GATT/descriptor/notification lifecycle, cancellation, mock TCK, system probe, package surface                                    | Mock D-Bus TCK, system probe, and live-radio/reliability evidence required by the label                                             | Live non-Noble Node/Electron scenario on each declared Linux distribution/adapter plus reliability evidence                  |
| CoreBluetooth desktop                            | Owned Node-API backend, public/core adapter, descriptor/advertisement mapping, cancellation quarantine, Node/Electron ABI gates, IPC integration                                 | Native mock/TCK, Node/Electron ABI, and live-radio evidence required by the label                                                   | Artifact-bound physical macOS Node/Electron vertical slice, packaging/signing and declared reliability coverage              |
| WinRT                                            | Owned TypeScript backend and protocol-v2 Node-API boundary for adapter/scan/connect/GATT/descriptors/CCCD, cancellation, terminal records, TCK registration, fail-closed loading | Mock TCK, native compile/Electron ABI, and live-radio evidence required by the label                                                | Current Windows compile/ABI evidence, physical Node/Electron radio slice, packaging/signing and declared architecture matrix |
| Electron IPC                                     | Versioned main/renderer handshake, sender authorization, renderer leases, ownership, bounded payload/stream handling, reload/rebind and cleanup scenarios                        | Deterministic IPC scenarios plus the selected desktop backend's required package/live proof                                         | Bind current packed-consumer and physical desktop runs; expand reload/crash/restart reliability evidence                     |
| Tauri v2 desktop                                 | Authenticated webview IPC, lease/replay/cancellation guards, bounded Channel delivery, packaged Rust plugin source, and a btleplug CoreBluetooth/WinRT/BlueZ dispatcher          | Deterministic manager/transport proof and macOS/Windows/Linux Rust compile/package proof; live radio is required above Experimental | Retain physical scan/connect/discover/read/write/sustained-notify/disconnect and reload/restart evidence per declared OS     |
| Meta Quest                                       | Not a 4.0 implementation or evidence target                                                                                                                                      | None for 4.0                                                                                                                        | Deferred to 4.1 with no 4.0 claim or gate                                                                                    |
| Controllable physical fault-injection peripheral | Deterministic fault injection exists; physical controller is not a 4.0 delivery item                                                                                             | Deterministic proof only in 4.0                                                                                                     | 4.1 feasibility, provider selection, procurement, and physical-radio scenarios                                               |

## Remaining evidence work

- Keep exact package/source bindings for evidence records whose label semantics require artifact identity.
- Run the physical Web, macOS, Linux, Windows, Android, and Apple scenarios needed for labels above the currently retained proof level. Hardware availability blocks only the associated label.
- Capture background, restoration, reconnect, renderer-restart, and soak records before promoting a backend to a label that promises those properties.
- Keep generated platform support documentation synchronized with retained records; never edit a support label by hand.
- Continue independent-consumer validation as a regression/supply-chain signal without treating a first-party consumer as public API authority.

These are ongoing backend qualification tasks. They are not a reason to manufacture evidence or to call the stable host-neutral package a prerelease.

## Release and evidence rules

- The evidence manifest is the source for generated platform-support pages and labels.
- A backend cannot report a capability without its typed implementation and required TCK profile.
- No static matrix, mock, or legacy `supports()` helper is a runtime source of truth.
- No first-party desktop claim may depend on a hidden Noble fallback.
- The 4.x publication has no permanent scoped shim, compatibility adapter, or Base64/bytes dual public API.
- Consumer-application evidence is a convergence signal, not public contract authority.
- Stable npm SemVer is governed by the package/API and release-integrity gates; backend labels remain governed by evidence.

## Historical issue mapping

Existing `GAP-*` labels in historical trackers/documents may continue to identify source locations or evidence records. When they refer to a Base64 bridge, `BlePort`, `PortBleManager`, static capability matrix, dual APIs, Noble wrapper, shim, or reduced scope, read them as historical characterization. New work should use the current 4.x public contract and maintained evidence model.

## Related records

- [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
- [`../ROADMAP.4.0.md`](../ROADMAP.4.0.md)
- [`../MIGRATION_4.0.md`](../MIGRATION_4.0.md)
- [`PLATFORMS.md`](PLATFORMS.md)
- [`../RELEASE.md`](../RELEASE.md)
