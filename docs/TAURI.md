# Tauri v2

Tauri desktop support uses the same versioned IPC contract and client policy as
Electron, while the Rust plugin owns the physical BLE adapter. Webview code
cannot select or load a Node native addon.

## Install

Install the JavaScript package and Tauri API in the application:

```sh
pnpm add unified-ble-manager @tauri-apps/api
```

The npm artifact contains the Rust crate source, so Cargo builds it for the
consumer's exact operating system and architecture. There is no Node addon in
the webview and no native binary download or postinstall script.

Reference the packaged crate from `src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }
```

## Frontend manager

Pass the official Tauri v2 core APIs explicitly:

```ts
import { Channel, invoke } from '@tauri-apps/api/core'
import { createTauriBleManager } from 'unified-ble-manager/tauri'

const manager = await createTauriBleManager({ invoke, Channel })
const state = await manager.adapterState()
const scan = await manager.scan({ serviceUuids: ['180d'] })
```

The transport calls the scoped `plugin:unified-ble-manager|invoke` command for
bounded request/response traffic and passes one Tauri `Channel` for streamed
BLE events. Event delivery remains lease-bound and acknowledged through the
same IPC authority.

`@tauri-apps/api` is deliberately not a dependency of the host-neutral package.
The consuming Tauri application already owns that runtime and passes its
official `invoke` and `Channel` exports to the transport.

`createTauriBleProvider({ invoke, Channel })` is also available when an
application wants a reusable host factory. Low-level `TauriBleIpcTransport` and
`TauriBleClient` exports remain available for protocol tests and custom hosts.

## Rust plugin and radio backend

Register the included production dispatcher:

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        )
        .build(),
    )
```

It uses CoreBluetooth on macOS, WinRT on Windows, and BlueZ/D-Bus on Linux.
Multiple-adapter hosts fail closed until trusted Rust configuration supplies an
exact `BtleplugDispatcherOptions.adapter_id`.

Grant the generated default permission only to intended application windows:

```json
{
  "identifier": "main",
  "windows": ["main"],
  "permissions": ["unified-ble-manager:default"]
}
```

## Security boundary

- Rust/plugin code owns the adapter, connections, and GATT resources.
- The webview owns no radio implementation or backend selection.
- The plugin authenticates the invoking webview/window rather than trusting
  caller-supplied identity fields.
- Attachment, version, replay, retained-byte, outstanding-operation, and lease
  checks use the shared framework-neutral IPC authority.
- Tauri command permissions must grant the plugin only to intended windows and
  webviews.

## Platform prerequisites

- macOS: add an appropriate Bluetooth usage description to the app bundle and
  grant Bluetooth permission when prompted.
- Windows: package with the Bluetooth capability required by the application.
- Linux: install BlueZ and grant the app/user access to its system D-Bus API;
  build hosts also need `pkg-config` and D-Bus development headers.

## Evidence status

The implementation and CI provide deterministic JavaScript proof plus Rust
compile/lint proof on macOS, Windows, and Linux. This is not physical-radio
evidence. The backend remains Experimental until retained scan/connect/GATT and
sustained-notification runs justify a higher support label. See
[`docs/GAPS.4.0.md`](GAPS.4.0.md).
