# tauri-plugin-unified-ble-manager

Cross-platform Tauri v2 BLE plugin for Unified BLE Manager 4.0. It authenticates
the invoking webview/window in Rust, exposes one permission-gated command, and
ships a production `btleplug` dispatcher for CoreBluetooth, WinRT, and BlueZ.

The documented consumer install is crates.io:

```toml
[dependencies]
tauri-plugin-unified-ble-manager = "5.0.0-rc.5"
```

```sh
cargo add tauri-plugin-unified-ble-manager@5.0.0-rc.5
```

The crate is not yet published. Until the crate is published, use the plugin,
`btleplug`, and `bluez-async` paths shipped by the exact npm package. The
consumer workspace root must declare both vendor entries under
`[patch.crates-io]`; Cargo ignores patch tables in dependency manifests. The
complete copy-and-paste layout is in [`../../docs/TAURI.md`](../../docs/TAURI.md).

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
