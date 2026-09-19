use tauri::Manager;

/// `UBM_TAURI_START_PAGE=driver.html` opens that page of the frontend instead
/// of the proof page, so automation (scripts/launch-driver-host.command) can
/// start the app as a test-driver host with no click.
const START_PAGE_ENV: &str = "UBM_TAURI_START_PAGE";

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_unified_ble_manager::PluginBuilder::new(
                tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
            )
            .build(),
        )
        .setup(|app| {
            if let Ok(page) = std::env::var(START_PAGE_ENV) {
                let window = app
                    .get_webview_window("main")
                    .ok_or("the main window is missing")?;
                let target = window.url()?.join(&page)?;
                window.navigate(target)?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Unified BLE Tauri example");
}
