<!-- docs/TAURI.md -->

# Tauri v2

Tauri webviews use `createTauriBleManager({ invoke, Channel })`. That returns an **`IpcBleManager`**, not the host-neutral `BleManager`. You cannot copy `BleManager.scan({ filter, delivery, sharing })` onto this object.

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
import { Channel, invoke } from '@tauri-apps/api/core'
import { createTauriBleManager } from 'unified-ble-manager/tauri'

const manager = await createTauriBleManager({ invoke, Channel })
const state = await manager.adapterState()
const scan = await manager.scan({ serviceUuids: ['180d'] })
```

`IpcBleManager.scan` takes optional `serviceUuids`, `manufacturerData`, `localNamePrefix`, `signal`, and `timeoutMs`. That is not `BleManager.scan`. Call `manager.scan()` with no options to hear every advertisement. Native filters are AND predicates: a `localNamePrefix` drops ads that omit a local name, which is common on CoreBluetooth.

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
