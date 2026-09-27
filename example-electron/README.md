<!-- example-electron/README.md -->

# Electron deterministic L1 smoke

This repository fixture verifies the packed 5.0.0-rc.12 contract surface without
claiming live Electron-radio support. It runs a deterministic scan, connect,
discover, read, notify, and destroy journey through the packed package. It does
not create an Electron application, load a native addon, or validate a physical
adapter/peripheral.

The smoke imports these public entrypoints:

- `unified-ble-manager/testing` for the deterministic scenario factory
- `unified-ble-manager/electron/main` for `ElectronMainBleRouter`

Composition sources (`composition-main.js`, `composition-preload.js`, `composition-renderer.js`) show the ownership
sequence: main owns the radio, preload exposes a narrow transport, the renderer
uses `createElectronRendererBleManager`. The low-level
`ElectronRendererBleClient` is internal to that factory and remains available
only for boundary-level tests. `node example-electron/composition.js` checks
those files without opening a window. That is not live-radio proof.

Run the L1 packed smoke from the repository root after producing the package artifacts:

```bash
pnpm prepack
node example-electron/smoke.js
node example-electron/composition.js
```

Success ends with `example-electron L1 smoke OK`. The deterministic boundary
is intentional: it makes this a repeatable package-surface and resource-cleanup
check, not a substitute for device-lab validation. It is L1 proof for the
published package/IPC surface only; it cannot promote Electron, CoreBluetooth,
WinRT, or BlueZ to a live support label. See [`../docs/ELECTRON.md`](../docs/ELECTRON.md)
for ABI and main/renderer integration, and [`../docs/PLATFORMS.md`](../docs/PLATFORMS.md)
for the current Experimental evidence boundary.

## Shared test driver (live Electron host)

`driver/` is a runnable Electron app that hosts the cross-host test scenarios.
Main (`driver/main.cjs`) selects one desktop backend explicitly, owns the radio
and installs `ElectronMainBleRouter` with `ElectronMainBleBinding`. The preload
(`driver/preload.cjs`) exposes the versioned BLE transport plus named,
ID-only process controls on a separate application channel; it never exposes
`ipcRenderer` or a filesystem configuration operation. One lazy process owner
serves borrowed renderer managers and the `process-continuation` scenario.
Startup and offline journal access do not acquire a radio. Journals use the
application's private user-data directory, never a renderer-supplied path.
Main lazily creates the recording subdirectory before live configuration or
offline access (new POSIX directories use mode `0700`; existing permissions
remain application-owned). Directory creation failures remain visible and
retryable; offline access does not acquire a radio.
The sandboxed
renderer runs the shared scenarios through `createElectronRendererBleManager`.
Running it is a manual live check, not a support label or evidence receipt.
Launch commands are in [`../examples-shared/driver/README.md`](../examples-shared/driver/README.md).

Quit seals new work and attempts all known owners, including the native parent,
without waiting for held application initialization; late resources remain tracked.
If either rejects or refuses release, the process stays alive with those owners
retained; request Quit again to retry. A failed cleanup never produces a
successful exit merely because its error was logged. Even successful radio
release cannot exit while continuation status is non-null: use an explicit
`process-continuation claim` to receive the retained backlog, then Quit again.
The window and separate handoff controls stay available during this retry.
These implementation tests are not physical-radio or packaged-runtime qualification.

An unexpected renderer termination triggers one deferred reload of the same
window's trusted local document, with a 30-second deadline. It preserves the
existing main-process session, native collection and authenticated bridge;
recovery does not execute another order, claim values, acknowledge journals or
clear data. The old frame remains unauthorized, and the replacement must pass
the normal frame/document checks. Normal shutdown and clean renderer exit do not
trigger navigation during shutdown. A crash during a refused shutdown stays
pending and can resume once that attempt settles; a skipped scheduled reload
does not consume the automatic attempt. Successful process exit never resumes it.
A second crash, failed load or deadline expiry is logged and
shown in the window title; automatic recovery does not loop. Use the trusted
main menu **Recovery → Recover renderer for handoff** to request one further
bounded attempt on the same window and owner. Repeated clicks while recovery is
pending coalesce; this action never resets the host or acknowledges data.
Recovery listeners are retired idempotently when the window closes, without
accessing the destroyed window's native properties during normal process exit.
Reload completion
proves navigation only, not resumed sensor delivery: explicitly query status and
perform the intended handoff. Already-running older app instances cannot acquire
this handler from a renderer reload.

For a real Electron renderer/preload/main smoke without BLE acquisition, build
the driver with `pnpm exec vite build --config example-electron/driver/vite.config.mts`,
then run `pnpm exec electron example-electron/driver/smoke-no-radio.cjs`. It checks
sandboxed controls, the idle claim, unknown-token rejection and normal Quit.
It does not prove radio or background data collection.
# Trusted BlueZ daemon policy

Launch trusted main with `--backend bluez --bluez-daemon-owner :1.N` or
`UBM_BLUEZ_DAEMON_OWNER=:1.N` (the flag wins). The value must be an explicitly
verified unique D-Bus owner implementing LE1 lifecycle methods, not a guessed
version or automatic attestation. The public factory validates it, and daemon
replacement requires fresh trusted configuration. Other backends reject it;
omission does not permit Linux connection acquisition and never enables a
Device1/legacy fallback. Main passes one policy to the process owner shared by
ordinary manager and continuation operations. Renderers/scenarios cannot choose
it, and offline journal access still opens no central.
