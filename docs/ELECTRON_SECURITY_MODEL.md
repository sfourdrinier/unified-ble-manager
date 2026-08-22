<!-- docs/ELECTRON_SECURITY_MODEL.md -->

# Electron security model

This page is the ownership and threat boundary for Electron. Consumer setup lives in [`ELECTRON.md`](ELECTRON.md). Nothing here is live-radio evidence.

## Ownership

- Main-process code selects and owns the radio/backend.
- Renderers use the versioned IPC client and never load a Node-API addon.
- `ElectronMainBleBinding` authenticates each `WebContents` from host facts.
- Security-sensitive IPC authority is a main-process snapshot of distinct
  `security:state`, `security:pair`, `security:cancel-pairing`,
  `security:unpair`, and `security:custom-ceremony` permissions; renderer
  payload fields cannot grant or mutate those permissions.
- Navigation, renderer destruction, app shutdown, and backend restart must drop renderer leases.

## Window policy

- `contextIsolation: true`
- `nodeIntegration: false`
- sandbox where the host allows it
- no generic `ipcRenderer` on `window`
- a Content-Security-Policy that does not unlock eval for untrusted pages
- unpack native addons from ASAR when the host requires it
- signing and notarization are the application’s release job, not this library’s proof

## Streams and generations

- Event acknowledgement and bounded streams apply on the IPC membrane.
- Generation quarantine: stale connection/database handles fail closed.
- Security command admission is checked before routing, and a scope change
  requires a new authenticated bootstrap; custom ceremonies remain rejected
  until a data-only challenge protocol exists.
- Unsupported threat claims (for example “this IPC path is immune to a compromised renderer”) are out of scope.

## Evidence

Deterministic packed smoke is L1 package/IPC proof. Native prebuild compilation and ABI loading are L2/L3. They do not promote CoreBluetooth, WinRT, or BlueZ to a live-radio label. See [`PLATFORMS.md`](PLATFORMS.md).
