<!-- docs/TAURI.md -->

# Tauri v2

Tauri webviews use the zero-plumbing `createTauriBleManager()` factory, which returns the public `BleManager`. The test-only `createTauriBleManagerWithEnvironment(...)` entrypoint accepts injected `invoke` and `Channel` implementations.

The Rust plugin owns the radio (btleplug: CoreBluetooth, WinRT, or BlueZ). The webview never loads a Node addon.

## Install

```sh
pnpm add unified-ble-manager @tauri-apps/api
```

The paired npm/Cargo registry distribution is a PR11 gate. Until that release
slice lands, repository consumers may use the checked-out plugin path only as
an explicitly interim development setup; do not present it as the published
installation recipe or as independent Cargo distribution proof.

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
