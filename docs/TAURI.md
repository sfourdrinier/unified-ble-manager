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
  filter: { serviceUuids: ['180d'], manufacturerData: [], localNamePrefix: null }
})
// consume scan.observations (value / overflow / terminal), then:
await scan.stop()
await manager.destroy()
```

`BleManager.scan` accepts the canonical filter shape plus `signal`, `timeoutMs`, and a stream preset. Native filters are AND predicates: a `localNamePrefix` drops ads that omit a local name, which is common on CoreBluetooth.

Remote streams use a drop-oldest policy with item capacity 128 and a 512 KiB byte bound. Overflow is a stream item, not a silent loss. GATT object parity and host-issued capability snapshots remain part of the in-progress PR2 migration.

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
