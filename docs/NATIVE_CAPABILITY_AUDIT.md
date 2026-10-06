# Native operation audit for the 5.x release

This is a source-path and deterministic-evidence inventory, not a physical-radio
qualification claim. Runtime capabilities remain authoritative; this document
does not replace them with a platform support matrix.

## Active operation boundaries

| Operation                                   | Android                                                                                     | Apple mobile / CoreBluetooth desktop                                                                           | Windows / WinRT                                                                | Linux / BlueZ                                                             | Web                                                                                                          |
| ------------------------------------------- | ------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Discovery and connect                       | Rust mobile admission routes to Android scanner/GATT callbacks and CDM where requested      | Owned CoreBluetooth central delegates; desktop patched btleplug central                                        | Rust central plus patched watcher and maintained GATT session                  | Rust central plus documented daemon LE-bearer/discovery authority         | Browser requestDevice chooser and GATT server; no continuous scanner                                         |
| GATT discovery and descriptors              | Android GATT discovery completion and descriptor callbacks                                  | CoreBluetooth discovery/descriptor delegates preserve occurrence identity                                      | Uncached service and characteristic queries preserve attribute identity; descriptor discovery does not call GetDescriptors or GetDescriptorsForUuid, so the descriptor list stays empty | Daemon authoritative discovery plus patched attribute identity            | Browser service/characteristic/descriptor promises; legitimate empty descriptor list is not an error         |
| Reads and writes                            | Native request callback reports dispatched outcome; write mode follows native request       | Read-versus-notify provenance is explicit; maximum write lengths/readiness come from CoreBluetooth             | Native communication status and selected response mode, not request intent     | D-Bus result and selected native write mode                               | Copies byte views; response/no-response browser methods selected explicitly                                  |
| Notifications and service change            | Subscription owner and native service-change callback invalidate generation                 | Delegate subscription ownership and service modification invalidation                                          | CCCD mode negotiation and ServicesChanged callback                             | Native notification stream and daemon discovery change authority          | Browser notifications and disconnect events; capabilities do not invent unavailable service-change authority |
| Security                                    | Android bond state/control and companion association report OS outcomes                     | CoreBluetooth has no explicit bond state/pair/unpair API; reports unsupported                                  | OS pairing/unpairing/status and cancellation ownership                         | Documented D-Bus/security capabilities and explicit privileged host hooks | Browser authorization is not bond control; unsupported where OS answer is unavailable                        |
| References, cancellation, power and release | Scoped mobile identities, tracked native cancellation, adapter observations, owned teardown | Scoped UUIDs and delegates; cancellation never fabricates physical release; Apple power control is unsupported | Adapter identity/watch and maintained-session cleanup retained on failure      | Adapter/watch and scoped release; no implicit privilege escalation        | Origin-authorized identity and browser-owned permission; no fabricated MAC or power control                  |

The active source anchors are `android/.../rustcore/RustRadioHostAdapter.kt`,
`ios/UnifiedBleRustRadioAdapter.swift`, `ios/Owned/`,
`crates/ubm-desktop/src/central.rs`, `crates/ubm-desktop/src/os/`,
`vendor/btleplug/src/{corebluetooth,winrtble}`, and `src/web/`. The retired
native Electron C++/Objective-C producers are not evidence for these routes.

## Concrete mechanisms completed in this pass

React Native public `choose()` now reaches real platform UI rather than being
withheld by the library: Apple AccessorySetupKit on eligible iOS applications,
and Android's existing CompanionDeviceManager association path. Both use
bounded admission, abort observation and explicit ownership. Android OR filters
map to CDM filters, preserving name-prefix, service and manufacturer matching.
The shared public selector policy rejects `acceptAllDevices: true` with nonempty
filters, and rejects explicit `false` without filters. Android and Web default
to unfiltered selection where supported; no constraint is silently discarded.
Apple requires declared accessory identity selectors and rejects undeclared or
unrepresentable requests before allocating an ASK session. No chooser result
claims a connection, advertisement name, restoration launch or radio receipt.
The React Native attachment owns pending ASK requests: destroy closes admission,
attempts native cancellation, retains refused cleanup for retry, and does not
allocate another picker after destruction. Android context invalidation closes
its owned picker activity before settling the callback.
See [background execution](BACKGROUND.md) for app declarations and ASK relaunch
limits. ASK is not available on tvOS, macOS or Mac Catalyst.

Selector validation is shared at the TypeScript boundary and repeated strictly
at native boundaries. The optional Android filter argument stays within the
existing `companion.associate` route; the regenerated UniFFI ABI adds its actual
fourth field. Production consumers must refresh owner-generated native artifacts
and cannot use an old binary with a new binding.

## Durable intake while JavaScript is suspended

Native continuation executors own collection independently of JavaScript.
`continuation_journal.rs` commits native ingress to SQLite before admitting it
as durable. Consumer preparation is an immutable prefix, not acknowledgement;
seal is not acknowledgement either. Acknowledgement commits deletion and its
receipt transaction before success is returned. An app's archive/database
commit must precede its explicit recording acknowledgement; UBM cannot infer
that external commit from delivery alone.

Regression boundaries include suspended-JS native collection, setup completion
held while values arrive, service-change rediscovery/resubscription without a
JavaScript wake, queue pressure, SQLite lock refusal, restart before/after ACK,
immutable prepared prefixes, late completion and retained failed cleanup.
Relevant executable suites are mobile `tests/continuation.rs`, desktop
`tests/continuation_journal.rs` / `continuation_durable_loss.rs`, and React
Native `continuation-recording.test.js`.

## Remaining evidence boundaries

Swift owner/selector harnesses, Apple SDK typechecks, Android JVM tests and Rust
fake-radio tests establish logic and compile boundaries only. They do not prove
system picker UI, entitlement approval, a force-quit relaunch, real-radio pairing
or background device collection. Existing ordinary restoration receipts do not
qualify the newly added ASK setup/relaunch mechanism. Windows and Linux runtime
qualification belongs to their maintained platform lanes; a source inventory
does not turn an unavailable local platform run into a pass. Support labels are
generated from retained artifact-bound evidence and remain unchanged.
