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
- `sandbox: true`; call `app.enableSandbox()` before Electron is ready
- no generic `ipcRenderer` on `window`
- a sandboxed preload may use Electron's preload API only to expose named,
  typed transport methods; it must not expose a generic IPC transport, addon
  loader, native store, or recording directory
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

## Native addon boundary

The Node-API addon is a trusted Node/main-host surface. A secure renderer uses
the public renderer entrypoint and the authenticated IPC client only. Electron
does not provide a standard Node-API proof that a loaded addon belongs to main:
a renderer or preload configured with Node/native-module access has been given
the embedding application's OS-user authority and is no longer an untrusted
renderer. UBM therefore does not use mutable JavaScript process globals as a
security check. Hosts must keep native addon loading and durable recording
objects in main, and expose only named, caller-bound IPC operations.

## Shared desktop peer-directory routes

Electron and Tauri use the existing authenticated IPC version4 envelope for
`peers.resolve`, `peers.known`, `peers.connected`, `peers.bonded`,
`peers.authorized` and `peers.restored`. These are additive read-only routes, not
a second transport or a protocol-version claim that every OS supports them.
Older hosts without a route reject it explicitly; no empty result or cached
substitute is synthesized. Runtime capability descriptors remain authoritative.

`resolve` carries a validated `reference` and returns `{peer: record | null}`.
Enumeration carries `query` with optional `sources`, canonical `services`,
validated `references` and `includeUnavailable`, returning `{peers: records}`.
Records preserve native reference, peer ID, name, RSSI, source and state; timestamps
are null when unknown, or retain an explicit backend clock scope. System-connected
membership must not be inferred from a process-local connection state or a scan.

The existing sender, renderer-lease, attachment-generation, correlation, message
quota, cancellation and relative-budget checks still apply. A query acquires no
connection lease; connecting a returned peer acquires its own lease normally.
Direct reference connect first resolves using the same signal and remaining
original deadline, and never connects when resolution is absent or fails.
Public query waits are bounded even if a host settles late; cancellation does
not claim the underlying OS lookup was actually cancelled. Native errors retain
their code, domain, operation and platform detail.

## Evidence

Deterministic packed smoke is L1 package/IPC proof. Native prebuild compilation and ABI loading are L2/L3. They do not promote CoreBluetooth, WinRT, or BlueZ to a live-radio label. See [`PLATFORMS.md`](PLATFORMS.md).
