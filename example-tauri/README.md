# Tauri v2 proving consumer

This deliberately small, vendor-neutral app exercises the public Tauri surface:

`adapter state -> scan -> connect -> discover -> read -> subscribe -> unsubscribe -> disconnect -> destroy`

The source checkout uses an explicitly interim relative Rust dependency. Paired
published npm/Cargo installation is a PR11 release gate and is not claimed by
this example; see [`../docs/TAURI.md`](../docs/TAURI.md).

`src/main.ts` is the public API proof source. `frontend/index.html` is a minimal,
dedicated static asset root for the cross-platform Rust compile gate. Keeping it
separate prevents Tauri from treating `src-tauri/target` as frontend content
while Cargo is writing that directory.

The first discovered peer is used only after the user presses **Run BLE proof**.
No device or vendor UUID is built into the library or example.
