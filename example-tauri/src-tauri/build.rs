fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["reference_process_continuation"]),
    ))
    .expect("Tauri application command permissions must be generated")
}
