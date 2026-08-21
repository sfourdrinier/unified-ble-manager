<!-- docs/TAURI.md -->

# Tauri v2

Tauri webviews use the zero-plumbing `createTauriBleManager()` factory, which returns the public `BleManager`. The test-only `createTauriBleManagerWithEnvironment(...)` entrypoint accepts injected `invoke` and `Channel` implementations.

The Rust plugin owns the radio (btleplug: CoreBluetooth, WinRT, or BlueZ). The webview never loads a Node addon.

## Install

```sh
pnpm add unified-ble-manager @tauri-apps/api
```

```toml
[dependencies]
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }
```

## Frontend

```ts
import { createTauriBleManager } from 'unified-ble-manager/tauri'

const manager = await createTauriBleManager()
const scan = await manager.scan({
  query: { anyOf: [{ services: { any: ['180d'] } }] },
  duplicates: 'coalesced',
  delivery: 'balanced'
})
// consume scan.observations (value / overflow / terminal), then:
await scan.stop()
await manager.destroy()
```

`BleManager.scan` accepts the frozen `ScanQuery` Boolean algebra plus `signal`, `timeoutMs`, duplicate policy, and a stream preset. Query matching is performed by the shared portable matcher; native projections are only safe broad prefilters.

Remote streams preserve bounded delivery and overflow notices. GATT objects are immutable, generation-bound views; btleplug reports notification delivery as unknown when it cannot select notification versus indication.

Filter in the webview with `advertisementPassesViewFilter` (name or peer id, min/max RSSI, service UUID, manufacturer company id, named-only). Observations include `serviceUuids`, `manufacturerData`, `txPowerLevel`, and `serviceData` in addition to `peerId` / `localName` / `rssi`.

## Rust plugin

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

See [`example-tauri/`](../example-tauri/) for a small public-API proof.

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
