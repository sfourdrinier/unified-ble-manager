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

`ScanClause.peers` is an additive scan predicate. It matches only observations carrying a trusted reference with the exact same backend, scope, and opaque identity. The matcher never derives a persisted reference from an address or an untrusted public ID.
