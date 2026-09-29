# Peer directories and persisted references

PR5 keeps peer knowledge explicit. A `PeerReference` is an opaque, versioned locator scoped to one backend and one application/origin; it is not a MAC address or a global hardware identity.

```ts
import { decodePeerReference, encodePeerReference } from 'unified-ble-manager'

const peer = await manager.peers.authorized()
const firstReference = peer[0]?.reference
const saved = firstReference == null ? null : encodePeerReference(firstReference)

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

Restoration directories: `peers.restored()` lists the peers the OS handed back after the app was gone — claimed on iOS via `restoration.claim()`, read directly on Android after `presence.observe({ peerId })` armed a Companion Device Manager wake for an associated peer. The task-ordered chain is in [`BACKGROUND.md`](BACKGROUND.md).

`manager.peers` exposes separate `known`, `connected`, `bonded`, `authorized`, and `restored` queries. A backend may report an individual category as unsupported. Web Bluetooth reports origin-authorized devices only when the browser exposes `navigator.bluetooth.getDevices()`; those references are origin-scoped and may represent disconnected or out-of-range devices. Electron and Tauri forward directory queries to their trusted host; they do not infer peer knowledge in the renderer. Support depends on the host's actual radio boundary, and an older host without these routes fails explicitly.

### CoreBluetooth desktop retrieval

On macOS, `peers.connected({ services: ['180d'] })` asks the existing
CoreBluetooth manager for system-connected peripherals matching **any** supplied
service UUID. The filter is required: an omitted or empty list fails
`capability.unsupported` at `peers.connected.services-required`. UBM never
substitutes an arbitrary service or returns a fabricated empty directory.

Retrieval is read-only. A returned peer's `state.connection: 'connected'` reports
membership in the OS-connected directory, not a connection owned by this UBM
manager. Its local CoreBluetooth peripheral may still be disconnected. Call
`manager.connect(peer)` to acquire your own lease, then release it normally.
Releasing that lease does not promise to drop a physical link held by another
application or the OS. Names come from CoreBluetooth; RSSI and last-seen time are
`null`, because this lookup is not an advertisement observation.

These peers carry application-scoped CoreBluetooth UUID references. Persist them
only for the same application and backend. `peers.resolve(reference)` retrieves
the identifier without scanning or connecting and returns `null` when the OS no
longer knows it. The resolved record reports `connection: 'unknown'`; identifier
knowledge does not establish a system connection. `peers.known({ references })`
resolves only the supplied references, not every peer known to macOS. Unfiltered
`known()` and service-filtered identifier lookup remain explicitly unsupported.
Bonded, authorized and restored desktop categories remain unsupported where no
native implementation supplies those facts.

The reference dashboard first makes the unfiltered query supported by Android.
Only the specific service-filter-required refusal above causes a visible retry
with Heart Rate Service `180d`, using the original deadline and signal. Permission
failures, timeouts and unrelated errors are not converted into that retry.

Supported directory operations, including `resolve`, accept `signal` and
`timeoutMs`. An already-aborted request is rejected before backend dispatch;
an abort or deadline while waiting rejects the query without publishing a late
result. These are read-only queries: rejecting the wait does not assert that an
underlying native lookup was cancelled. Late completion remains observed, and
backend failures that settle first keep their specific error identity.

React Native directory observations preserve the native host's elapsed-time
`lastSeenAtMonotonicMs`. Their internal clock scope is stable within one backend
instance and distinct across instances; these values are not JavaScript clock
timestamps and must not be compared across managers or process lifetimes.

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

The Android operations currently carry deterministic evidence and therefore
report `state: 'limited'` until separate physical-radio qualification is
retained. They are still implemented and invocable, so
`manager.capabilities.supports('connection:when-available')` returns `true`.
Inspect `manager.capabilities.get(...)` when the distinction between
`supported` and `limited` affects product policy.

Permission failures are reported as `permission.denied`; they are not converted into an empty list. Android, Apple React Native, CoreBluetooth, BlueZ, WinRT, Web Bluetooth, Electron, and Tauri only advertise peer categories backed by their current native boundary. In particular, Web origin-authorized devices are not Android-style bonded peers, and unsupported categories fail with `capability.unsupported` rather than returning fabricated data.

`ScanClause.peers` is an additive scan predicate. It matches only observations carrying a trusted reference with the exact same backend, scope, and opaque identity. The matcher never derives a persisted reference from an address or an untrusted public ID.
