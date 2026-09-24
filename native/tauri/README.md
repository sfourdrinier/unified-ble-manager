# tauri-plugin-unified-ble-manager

Cross-platform Tauri v2 BLE plugin for Unified BLE Manager 4.0. It authenticates
the invoking webview/window in Rust, exposes one permission-gated command, and
ships a production `btleplug` dispatcher for CoreBluetooth, WinRT, and BlueZ.

The current consumer install uses the plugin and vendor patches from the same
packed npm package. Put these entries in the app's `src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }

# Cargo reads patches only from the consuming workspace root.
[patch.crates-io]
btleplug = { path = "../node_modules/unified-ble-manager/vendor/btleplug" }
bluez-async = { path = "../node_modules/unified-ble-manager/vendor/bluez-async" }
```

The crate is not yet published. Cargo ignores patch tables in dependency
manifests. `ubm init --host tauri --dir src-tauri` generates this recipe;
the complete setup is in [`../../docs/TAURI.md`](../../docs/TAURI.md).

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
