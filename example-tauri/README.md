# Tauri v2 proving consumer

This small checkout app uses `createTauriBleManager()` from
`unified-ble-manager/tauri`. The window button runs `adapter.state()`, then
`scan()`, then `stop()` and `destroy()`. It does not connect, discover, or
subscribe. A successful click is not live-radio evidence.

The Cargo registry crate is not published. Use the checkout path for local
development (`path = "../../native/tauri"`). A future crates.io publication may
use `cargo add tauri-plugin-unified-ble-manager@5.0.0-rc.1`; it is not part of
the npm release candidate.
See [`../docs/TAURI.md`](../docs/TAURI.md).

`src/main.ts` is the source of the button handler. `frontend/index.html` is a
static asset root for the Rust compile gate and does not load that script.
Keeping it separate prevents Tauri from treating `src-tauri/target` as frontend
content while Cargo is writing that directory.

The first discovered peer is used only after the user presses **Run BLE proof**.
No device or vendor UUID is built into the library or example.

## Shared test driver

`driver.html` and `src/driver.ts` host the cross-host test scenarios over the
Tauri IPC client (`createTauriBleManager`). The Rust plugin owns the radio. For
development, run the webview frontend with
`pnpm exec vite --config example-tauri/vite.config.mts` and the app with
`cargo run --manifest-path example-tauri/src-tauri/Cargo.toml` (debug builds load
`build.devUrl`), then open the driver from the window. Launch commands and the
protocol are in [`../examples-shared/driver/README.md`](../examples-shared/driver/README.md).
