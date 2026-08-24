# tauri-plugin-unified-ble-manager

Cross-platform Tauri v2 BLE plugin for Unified BLE Manager 4.0. It authenticates
the invoking webview/window in Rust, exposes one permission-gated command, and
ships a production `btleplug` dispatcher for CoreBluetooth, WinRT, and BlueZ.

The documented consumer install is crates.io:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = "4.0.0"
```

```sh
cargo add tauri-plugin-unified-ble-manager@4.0.0
```

The crate is not yet published. Until the crate is published, a repository
checkout may still use a path dependency for local plugin development.

Register the production dispatcher:

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        )
        .build(),
    )
```

When a host has more than one Bluetooth adapter, select one in trusted Rust
configuration with `BtleplugDispatcherOptions { adapter_id: Some(...) }`. The
renderer cannot select or forge adapter authority. Custom deterministic or
platform-native dispatchers can implement `IpcDispatcher` without changing the
webview protocol.

Grant `unified-ble-manager:default` only to the intended windows/webviews in the
application's Tauri capability file.
