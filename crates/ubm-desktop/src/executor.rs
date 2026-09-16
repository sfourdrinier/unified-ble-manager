//! Shared host-neutral desktop executor (HOST-DESKTOP).
//!
//! One process-wide Tokio runtime backs every desktop BLE manager. Hosts
//! must never build one runtime per manager: runtime-per-manager leaks the
//! per-runtime threads and strands orphan cleanup on abandoned executors.
//! [`desktop_runtime`] hands out the single shared handle, and
//! [`shutdown_desktop_runtime`] performs the explicit shutdown step that
//! stops admission of new desktop BLE work.
//!
//! SEAM COMPATIBILITY: the Tauri shell (`native/tauri`) includes this exact
//! file with `#[path]` so desktop execution stays single-sourced without a
//! Tauri dependency here and without touching the Tauri manifest there.
//! Keep this file self-contained (`std` + `tokio` only, no `crate::`
//! imports) and avoid syntax newer than Rust edition 2021 so it still
//! compiles inside the Tauri crate (edition 2021).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Static handle for the one shared desktop runtime.
static SHARED_HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Explicit-shutdown latch. Set once; never cleared for the process.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// Return the shared desktop runtime handle, building the runtime on first
/// use. The runtime lives on a dedicated thread (two workers) so desktop BLE
/// work never depends on — and never blocks — a host-owned runtime.
pub fn desktop_runtime() -> tokio::runtime::Handle {
    SHARED_HANDLE
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            // Thread/panic identity is retained verbatim from the 4.x Tauri path
            // (pinned by __tests__/TauriRustPlugin.test.js); revisit only if a
            // second desktop client onboards this shared executor.
            std::thread::Builder::new()
                .name("ubm-btleplug".to_owned())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .worker_threads(2)
                        .thread_name("ubm-btleplug-worker")
                        .build()
                        .expect("unified-ble-manager btleplug runtime");
                    tx.send(runtime.handle().clone())
                        .expect("unified-ble-manager btleplug handle");
                    runtime.block_on(std::future::pending::<()>());
                })
                .expect("unified-ble-manager btleplug thread");
            rx.recv().expect("unified-ble-manager btleplug handle")
        })
        .clone()
}

/// Explicit shutdown: stop admission of new desktop BLE work. Already
/// spawned radio work runs to its own cleanup; this latch only gates new
/// admission through [`checked_desktop_runtime`].
pub fn shutdown_desktop_runtime() {
    SHUT_DOWN.store(true, Ordering::SeqCst);
}

/// Whether explicit shutdown has been recorded for this process.
pub fn is_desktop_runtime_shut_down() -> bool {
    SHUT_DOWN.load(Ordering::SeqCst)
}

/// Admission guard: refuse new desktop BLE work after explicit shutdown so
/// teardown cannot be raced by a late starter. The error is a plain string
/// because this seam file carries no error-type dependency; callers map it
/// to their own contract identity (`adapter.unavailable`).
pub fn checked_desktop_runtime(operation: &str) -> Result<tokio::runtime::Handle, String> {
    if is_desktop_runtime_shut_down() {
        return Err(format!("{operation}: desktop executor shut down"));
    }
    Ok(desktop_runtime())
}

#[cfg(test)]
mod tests {
    use super::{checked_desktop_runtime, desktop_runtime};

    #[test]
    fn shared_handle_is_stable_across_calls() {
        // Handle equality is not exposed; identical task routing proves the
        // seam returns one runtime: both spawns complete on the same build.
        let first = desktop_runtime();
        let second = desktop_runtime();
        assert_eq!(first.id(), second.id(), "one shared runtime per process");
        assert!(
            checked_desktop_runtime("test.admission").is_ok(),
            "admission open before shutdown"
        );
    }
}
