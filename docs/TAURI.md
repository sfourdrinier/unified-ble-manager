<!-- docs/TAURI.md -->

# Tauri v2

Tauri webviews use the zero-plumbing `createTauriBleManager()` factory, which returns the public `BleManager`. The test-only `createTauriBleManagerWithEnvironment(...)` entrypoint accepts injected `invoke` and `Channel` implementations.

The Rust plugin owns the radio (btleplug: CoreBluetooth, WinRT, or BlueZ). The webview never loads a Node addon.

## Install

```sh
pnpm add unified-ble-manager @tauri-apps/api
```

Until the crate is on crates.io, point Cargo at the plugin in this repository:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = { path = "../../native/tauri" }
```

The intended published recipe is `cargo add tauri-plugin-unified-ble-manager@4.0.0`.
That command fails today because the crate is not published. `ubm init --host tauri`
writes the crates.io fragment so you can switch when it is.

## Frontend

```ts
import { createTauriBleManager } from 'unified-ble-manager/tauri'

const abort = new AbortController()
const manager = await createTauriBleManager()
const scan = await manager.scan({
  query: { anyOf: [{ services: { any: ['180d'] } }] },
  duplicates: 'coalesced',
  delivery: 'balanced',
  signal: abort.signal,
  timeoutMs: 15_000
})
const first = await scan.observations[Symbol.asyncIterator]().next()
await scan.stop()
if (first.done || first.value.kind !== 'value') {
  await manager.destroy()
  throw new Error('No peer observed')
}
const connection = await manager.connect(first.value.peer, { signal: abort.signal, timeoutMs: 10_000 })
try {
  const gatt = await connection.discover({ signal: abort.signal, timeoutMs: 10_000 })
  const battery = gatt.services.find(service => service.uuid === '180f')
  const level = battery?.characteristics.find(characteristic => characteristic.uuid === '2a19')
  if (level !== undefined) {
    const bytes = await level.read({ signal: abort.signal, timeoutMs: 5_000 })
    void bytes
  }
} finally {
  await connection.release()
  await manager.destroy()
}
```

`BleManager.scan` accepts the frozen `ScanQuery` Boolean algebra plus `signal`, `timeoutMs`, duplicate policy, and a stream preset. Query matching is performed by the shared portable matcher; native projections are only safe broad prefilters.

Remote streams preserve bounded delivery and overflow notices. GATT objects are immutable, generation-bound views; btleplug reports notification delivery as unknown when it cannot select notification versus indication.

Filter in the webview with `advertisementPassesViewFilter` (name or peer id, min/max RSSI, service UUID, manufacturer company id, named-only). Observations include `serviceUuids`, `manufacturerData`, `txPowerLevel`, and `serviceData` in addition to `peerId` / `localName` / `rssi`.

## Rust plugin (interim checkout setup)

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        )
        .build(),
    )
```

Grant `unified-ble-manager:default` only to intended windows. Await `manager.destroy()` when the webview session ends.

Security permissions are separate from the default transport permission. The
plugin defines `unified-ble-manager:allow-security-state`,
`allow-security-pair`, `allow-security-cancel-pairing`,
`allow-security-unpair`, and `allow-security-custom-ceremony`; each is enforced
by a Rust command scope, never by renderer request fields. The default set does
not grant unpair or custom-ceremony authority. The current btleplug dispatcher
still reports all generic security capabilities as unsupported until a native
pairing implementation and matching TCK/evidence are added.

See [`example-tauri/`](../example-tauri/) for a small public-API proof.

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
