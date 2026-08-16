# Tauri v2

Tauri desktop support uses the same versioned IPC contract and client policy as
Electron, while the Rust plugin owns the physical BLE adapter. Webview code
cannot select or load a Node native addon.

## Frontend transport

Pass the official Tauri v2 core APIs explicitly:

```ts
import { Channel, invoke } from '@tauri-apps/api/core'
import { TauriBleClient, TauriBleIpcTransport } from 'unified-ble-manager/tauri'

const transport = new TauriBleIpcTransport({ invoke, Channel })
const client = new TauriBleClient(transport)

await client.initialize()
```

The transport calls the scoped `plugin:unified-ble-manager|invoke` command for
bounded request/response traffic and passes one Tauri `Channel` for streamed
BLE events. Event delivery remains lease-bound and acknowledged through the
same IPC authority.

`@tauri-apps/api` is deliberately not a dependency of the host-neutral package.
The consuming Tauri application already owns that runtime and passes its
official `invoke` and `Channel` exports to the transport.

## Rust plugin

The npm artifact includes the publishable Tauri v2 crate source. Until the crate
is released independently on crates.io, reference that exact source from the
application:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }
```

Register the plugin with the native IPC dispatcher:

```rust,ignore
tauri::Builder::default()
    .plugin(tauri_plugin_unified_ble_manager::PluginBuilder::new(dispatcher).build())
```

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

The physical platform dispatcher and full `BleManager` proxy remain required
4.0 slices tracked in
[`#19`](https://github.com/sfourdrinier/unified-ble-manager/issues/19). The
plugin boundary alone does not claim physical-radio support.
