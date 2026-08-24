# Tauri v2 proving consumer

This small checkout app uses `createTauriBleManager()` from
`unified-ble-manager/tauri`. The window button runs `adapter.state()`, then
`scan()`, then `stop()` and `destroy()`. It does not connect, discover, or
subscribe. A successful click is not live-radio evidence.

The Cargo recipe you can copy today is the checkout path
(`path = "../../native/tauri"`). `cargo add tauri-plugin-unified-ble-manager@4.0.0`
is the published recipe once the crate exists; it fails until then.
See [`../docs/TAURI.md`](../docs/TAURI.md).

`src/main.ts` is the source of the button handler. `frontend/index.html` is a
static asset root for the Rust compile gate and does not load that script.
Keeping it separate prevents Tauri from treating `src-tauri/target` as frontend
content while Cargo is writing that directory.

The first discovered peer is used only after the user presses **Run BLE proof**.
No device or vendor UUID is built into the library or example.
