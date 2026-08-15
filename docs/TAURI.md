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

## Security boundary

- Rust/plugin code owns the adapter, connections, and GATT resources.
- The webview owns no radio implementation or backend selection.
- The plugin authenticates the invoking webview/window rather than trusting
  caller-supplied identity fields.
- Attachment, version, replay, retained-byte, outstanding-operation, and lease
  checks use the shared framework-neutral IPC authority.
- Tauri command permissions must grant the plugin only to intended windows and
  webviews.

The Rust crate, generated Tauri permissions, physical platform backends, and
full `BleManager` proxy are tracked as required 4.0 slices in
[`#19`](https://github.com/sfourdrinier/unified-ble-manager/issues/19). This
document does not claim physical-radio support until those native gates land.
