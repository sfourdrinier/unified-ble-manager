# Tauri v2 proving consumer

This small checkout app uses `createTauriBleManager()` from
`unified-ble-manager/tauri`. The window button runs `adapter.state()`, then
`scan()`, connects to the first peer, discovers GATT, and reads the Battery
Level characteristic through `gatt.characteristic('180f', '2a19')` before
releasing every resource. A successful click is not live-radio evidence.

The Cargo registry crate is not published. This repository example uses the
checkout path for local development (`path = "../../native/tauri"`). External
apps use the plugin and vendor patches from their exact installed npm package.
See [`../docs/TAURI.md`](../docs/TAURI.md).

`src/main.ts` is the source of the button handler. `frontend/index.html` is a
static asset root for the Rust compile gate and does not load that script.
Keeping it separate prevents Tauri from treating `src-tauri/target` as frontend
content while Cargo is writing that directory.

The first discovered peer is used only after the user presses **Run BLE proof**.
The example asks that peer for the standard Battery Service; it does not target
a device or vendor-specific UUID.

## Shared test driver

`driver.html` and `src/driver.ts` host the cross-host test scenarios over the
Tauri IPC client (`createTauriBleManager`). The Rust plugin owns the radio. For
development, run the webview frontend with
`pnpm exec vite --config example-tauri/vite.config.mts` and the app with
`cargo run --manifest-path example-tauri/src-tauri/Cargo.toml` (debug builds load
`build.devUrl`), then open the driver from the window. Launch commands and the
protocol are in [`../examples-shared/driver/README.md`](../examples-shared/driver/README.md).
