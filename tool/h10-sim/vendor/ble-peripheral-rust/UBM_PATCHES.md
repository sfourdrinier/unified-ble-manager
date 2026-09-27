# Simulator transport patches to ble-peripheral-rust 0.2.0

This is the crates.io 0.2.0 source from
https://github.com/rohitsangwan01/ble-peripheral-rust, retained under its original
MIT `LICENSE`. Only `src/`, the normalized manifest, and the license were copied;
examples and their unused logging dev-dependency were omitted. The simulator's
standalone Cargo workspace selects this copy by path. It is not the UBM central
backend and is not shipped as a production central implementation.
The copy is excluded from automatic workspace membership to avoid expanding
simulator lint checks into unchanged upstream code. A shared workspace dependency
and development dependency keep its focused tests runnable on Linux as well as
the platforms that use it in production.

Changes:

- `notification_payload_capacity` exposes the minimum current subscriber value
  capacity. CoreBluetooth reads `CBCentral.maximumUpdateValueLength`; WinRT reads
  `GattSubscribedClient.MaxNotificationSize`. No subscribers is `None`; unknown
  attributes, failed queries, and zero OS budgets are errors. BlueZ's adapter
  reads the existing writer budget (the simulator uses its own direct bluer
  transport on Linux).
- `update_characteristic` returns `NotificationOutcome`. CoreBluetooth's
  `updateValue` Boolean is retained: false is backpressure, never acceptance.
  No subscribed central is explicitly `NotSubscribed`. Oversized values fail
  before submission. The simulator reports refusal rather than retrying a
  command whose stream state could have changed.
- WinRT inspects every `GattClientNotificationResult`, retaining each refused
  recipient's status and protocol-error query result. Partial acceptance is an
  error, never a whole-broadcast acceptance. The original missing-characteristic
  panic is an explicit error. WinRT OS acceptance does not prove peer receipt.
- The otherwise-unused BlueZ crate adapter awaits and propagates its actual
  writer result instead of spawning a fire-and-forget task and reporting success.
- Pure notification policy tests cover smallest capacity, no subscribers, zero
  budget, backpressure, and partial/multiple recipient failures.

Validation commands from the repository root:

```
cargo test --manifest-path tool/h10-sim/Cargo.toml -p ble-peripheral-rust --lib
cargo check --manifest-path tool/h10-sim/Cargo.toml --target x86_64-pc-windows-msvc
```

These are deterministic/compile checks, not Windows or macOS peripheral-radio
qualification. The original upstream delegate lifecycle remains upstream code;
this patch does not claim a broader audit of that crate.
