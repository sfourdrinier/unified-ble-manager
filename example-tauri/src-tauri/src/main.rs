fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_unified_ble_manager::PluginBuilder::new(
                tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
            )
            .build(),
        )
        .run(tauri::generate_context!())
        .expect("error while running Unified BLE Tauri example");
}
