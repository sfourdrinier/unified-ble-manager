# Peer directories and persisted references

PR5 keeps peer knowledge explicit. A `PeerReference` is an opaque, versioned locator scoped to one backend and one application/origin; it is not a MAC address or a global hardware identity.

```ts
import { decodePeerReference, encodePeerReference } from 'unified-ble-manager'

const peer = await manager.peers.authorized()
const saved = peer[0]?.reference === null ? null : encodePeerReference(peer[0].reference)

if (saved !== null) {
  const reference = decodePeerReference(saved)
  const resolved = await manager.peers.resolve(reference)
  if (resolved !== null) {
    await manager.withConnection(resolved, { timeoutMs: 15_000 }, async connection => {
      await connection.discover()
    })
  }
}
```

`PeerReference` persistence belongs to the application. The library does not write storage or silently migrate references. Future reference versions fail with `peer.reference-version-unsupported`; malformed references fail before radio work.

`manager.peers` exposes separate `known`, `connected`, `bonded`, `authorized`, and `restored` queries. A backend may report an individual category as unsupported. Web Bluetooth reports origin-authorized devices only when the browser exposes `navigator.bluetooth.getDevices()`; those references are origin-scoped and may represent disconnected or out-of-range devices. Tauri reports the directory unsupported until its host boundary can provide truthful references.

React Native Android supports `bonded()` and `resolve()` through the Android system bond table. The app must request `BLUETOOTH_CONNECT` (and `BLUETOOTH_SCAN` for scanning) before calling them. A bonded peer is paired metadata, not proof that the radio is reachable: Android reports reachability as `unknown` and only reports `connection: 'connected'` when the manager already owns that live connection. Save the returned version-1, system-scoped reference and resolve it again before reconnecting:

```ts
const bonded = await manager.peers.bonded()
const savedReference = bonded[0]?.reference
if (savedReference != null) {
  const current = await manager.peers.resolve(savedReference)
  if (current !== null) {
    const connection = await manager.connect(current, { intent: 'when-available', timeoutMs: 15_000 })
    await connection.disconnect()
  }
}
```

Permission failures are reported as `permission.denied`; they are not converted into an empty list. Android, Apple React Native, CoreBluetooth, BlueZ, WinRT, Web Bluetooth, Electron, and Tauri only advertise peer categories backed by their current native boundary. In particular, Web origin-authorized devices are not Android-style bonded peers, and unsupported categories fail with `capability.unsupported` rather than returning fabricated data.

`ScanClause.peers` is an additive scan predicate. It matches only observations carrying a trusted reference with the exact same backend, scope, and opaque identity. The matcher never derives a persisted reference from an address or an untrusted public ID.
