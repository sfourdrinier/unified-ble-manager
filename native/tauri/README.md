# tauri-plugin-unified-ble-manager

Tauri v2 plugin boundary for Unified BLE Manager 4.0. It authenticates the
invoking webview/window in Rust, exposes one permission-gated command, and
delegates the shared versioned IPC protocol to an injected native dispatcher.

During the 4.0 release stack the crate is consumed directly from the npm package:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }
```

Register the plugin with a native dispatcher:

```rust,ignore
tauri::Builder::default()
    .plugin(tauri_plugin_unified_ble_manager::PluginBuilder::new(dispatcher).build())
```

Grant `unified-ble-manager:default` only to the intended windows/webviews in the
application's Tauri capability file.
