use tauri::Manager;
mod process_continuation;
mod trusted_policy;

/// `UBM_TAURI_START_PAGE=driver.html` opens that page of the frontend instead
/// of the proof page, so automation (scripts/launch-driver-host.command) can
/// start the app as a test-driver host with no click.
const START_PAGE_ENV: &str = "UBM_TAURI_START_PAGE";

fn main() {
    let adapter = match std::env::var("UBM_TAURI_ADAPTER") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid trusted adapter environment: {error}"),
    };
    let adapter_id =
        trusted_policy::adapter_id(adapter).expect("invalid trusted adapter launch configuration");
    let owner = match std::env::var("UBM_BLUEZ_DAEMON_OWNER") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid trusted BlueZ daemon owner environment: {error}"),
    };
    let connection_policy = trusted_policy::connection_policy(owner, cfg!(target_os = "linux"))
        .expect("invalid trusted BlueZ launch configuration");
    let dispatcher = tauri_plugin_unified_ble_manager::BtleplugDispatcher::new(
        tauri_plugin_unified_ble_manager::BtleplugDispatcherOptions {
            connection_policy,
            adapter_id,
        },
    );
    let process_dispatcher = dispatcher.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_unified_ble_manager::PluginBuilder::new(dispatcher).build())
        .invoke_handler(tauri::generate_handler![
            process_continuation::reference_process_continuation
        ])
        .on_page_load(|webview, payload| {
            if webview.window().label() == "main" {
                if let Some(state) = webview
                    .app_handle()
                    .try_state::<process_continuation::ProcessContinuation>()
                {
                    state.page_load(payload.event() == tauri::webview::PageLoadEvent::Started);
                }
            }
        })
        .setup(move |app| {
            let window = app
                .get_webview_window("main")
                .ok_or("the main window is missing")?;
            let document = window.url()?.join("driver.html")?;
            app.manage(process_continuation::ProcessContinuation::new(
                process_dispatcher.clone(),
                app.path().app_data_dir()?.join("ubm-continuation"),
                document,
            ));
            if let Ok(page) = std::env::var(START_PAGE_ENV) {
                let window = app
                    .get_webview_window("main")
                    .ok_or("the main window is missing")?;
                let target = window.url()?.join(&page)?;
                window.navigate(target)?;
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Unified BLE Tauri example")
        .run(|app, event| {
            let state = app.state::<process_continuation::ProcessContinuation>();
            if state.may_exit() {
                return;
            }
            match event {
                tauri::RunEvent::ExitRequested { api, .. } => {
                    api.prevent_exit();
                    process_continuation::ProcessContinuation::request_quit(app.clone());
                }
                tauri::RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::CloseRequested { api, .. },
                    ..
                } if label == "main" => {
                    api.prevent_close();
                    process_continuation::ProcessContinuation::request_quit(app.clone());
                }
                _ => {}
            }
        });
}
