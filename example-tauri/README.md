# Tauri v2 proving consumer

This small checkout app uses `createTauriBleManager()` from
`unified-ble-manager/tauri`. The window button runs `adapter.state()`, then
`scan()`, connects to the first peer, discovers GATT, and reads the Battery
Level characteristic through `gatt.characteristic('180f', '2a19')` before
releasing every resource. A successful click is not live-radio evidence.

The scan stream's value envelope contains the observation and its peer. Battery
bytes are checked by the shared Battery Level codec (one byte, 0–100 percent).
Peer observation has one ten-second budget, including scan admission. Overflow
notices are reported while waiting; source terminal errors retain their cause.
Each owned cleanup is attempted even if another fails; failures remain visible
and retained for the next click to retry before acquiring another manager. The
button is re-enabled after factory, operation, or cleanup failure.

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

The driver's `process-continuation` scenario uses an application-scoped Rust
command, not the BLE attachment bootstrap. It shares the plugin's retained
dispatcher/native engine; webview reload does not dispose that process owner.
`execute` starts an explicit known-peer HR/ECG/ACC recipe, not an OS proximity
wake. `status`, `claim`, and `stop` preserve actual handoff/cleanup receipts.
Renderer-side public codecs decode a prepared prefix before a separate ACK;
a transport/decode failure leaves the prepared prefix available for replay.
Durable recording ACK and clear remain explicit commands, never automatic.

The command accepts only the loaded `main` webview's exact local `driver.html`
document (query/hash do not change its identity), fences navigation and late
replies, and bounds outstanding requests to eight. Requests are limited to
512 KiB and responses to 8 MiB. No command accepts a filesystem path. Rust
selects `app_data_dir()/ubm-continuation`, creates new Unix directories with
mode 0700, and performs directory/SQLite work off the UI thread. Existing
directory permissions and OS backup/protection policy remain host deployment
responsibilities; journal storage is not encrypted. Offline journal commands
do not initialize Bluetooth. These deterministic checks are not physical-radio
or OS-background qualification.

User close/quit requests first close BLE admission and attempt the authoritative
parent cleanup. A refused cleanup or any non-null native handoff keeps the app
and main window alive, with a JSON diagnostic on stderr. Explicitly inspect/
export and claim the handoff, then retry close; an empty queue alone does not
authorize exit. Quit never implicitly prepares, acknowledges, or clears data.
This controlled-exit policy cannot retain volatile state after an unexpected
OS kill, crash, or power loss; durable journals require explicit recording opt-in.
# Trusted BlueZ daemon policy

On Linux, launch the trusted Rust application with
`UBM_BLUEZ_DAEMON_OWNER=:1.N`, substituting an explicitly verified unique D-Bus
owner implementing LE1 lifecycle methods. Rust configures the retained dispatcher
once; ordinary managers and process continuation share it. Native admission
validates the unique name and binds the policy to that daemon lifetime. No
automatic attestation or Device1/legacy fallback occurs; omission leaves Linux
connection acquisition unavailable. Non-Linux launches reject this environment
option. A daemon restart requires a new trusted launch configuration. This is not
a renderer command or scenario input, and offline journal access remains free of
central acquisition.
