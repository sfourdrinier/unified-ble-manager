# Tauri v2 proving consumer

This deliberately small, vendor-neutral app exercises the public Tauri surface:

`adapter state -> scan -> connect -> discover -> read -> subscribe -> unsubscribe -> disconnect -> destroy`

The documented consumer recipe is `cargo add tauri-plugin-unified-ble-manager@4.0.0`.
The crate is not yet published. Until the crate is published, this example keeps
the interim relative Rust path dependency. See [`../docs/TAURI.md`](../docs/TAURI.md).

`src/main.ts` is the public API proof source. `frontend/index.html` is a minimal,
dedicated static asset root for the cross-platform Rust compile gate. Keeping it
separate prevents Tauri from treating `src-tauri/target` as frontend content
while Cargo is writing that directory.

The first discovered peer is used only after the user presses **Run BLE proof**.
No device or vendor UUID is built into the library or example.
