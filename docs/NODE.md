<!-- docs/NODE.md -->

# Node.js

The root import does not open an adapter. Pick the entrypoint for your OS:

| Import                                   | Host    | Radio underneath                    |
| ---------------------------------------- | ------- | ----------------------------------- |
| `unified-ble-manager/node/corebluetooth` | macOS   | shared Rust core over CoreBluetooth |
| `unified-ble-manager/node/winrt`         | Windows | shared Rust core over WinRT         |
| `unified-ble-manager/node/bluez`         | Linux   | shared Rust core over BlueZ         |

All three execute one shared Rust core (`DesktopCentral` in `crates/ubm-desktop`, btleplug plus narrow OS adapters) through one N-API addon. This source targets `5.0.0-rc.20`. Tagged releases ship the addon prebuilt for `linux-x64`, `linux-arm64`, `darwin-arm64`, `win32-x64` and `win32-arm64`, under `native/desktop-core/prebuilds/<platform>-<arch>/`. A normal install compiles nothing and needs no Rust toolchain. The app no longer needs `dbus-next` on Linux.

macOS desktop support is Apple Silicon (`arm64`) only. Windows and Linux desktop support includes `arm64` and `x64`.
Intel macOS desktop is outside the UBM support policy; this is a package policy,
not a claim that Apple no longer supports every Intel macOS version. It does not
provide an Intel desktop source-build support path. iOS/tvOS simulators are `arm64` only;
physical iPhone support is unchanged. The simulator policy requires an Apple Silicon Mac,
not an Intel simulator or a Rosetta workaround.

Runtime requirements:

- **Linux:** glibc 2.35 or newer, and `libdbus-1.so.3` (package `libdbus-1-3`). musl is not supported: loading fails with `no-prebuilt-for-target`, and the detected libc is in the error. A `dlopen` failure, such as a missing `libdbus`, is reported verbatim.
- **macOS:** the process must hold Bluetooth permission (TCC). A process without it is terminated by macOS when the radio opens. That is a macOS policy, not a library failure.
- **Windows:** Windows 10 1809 or newer with a Bluetooth LE adapter.

## One-call factories

```ts
import { createCoreBluetoothBleManager } from 'unified-ble-manager/node/corebluetooth'
import { createWinRtBleManager } from 'unified-ble-manager/node/winrt'
import { createBluezBleManager } from 'unified-ble-manager/node/bluez'

const manager = await createCoreBluetoothBleManager()
```

Backend and adapter ids are the 4.x ones: backend ids `unified-ble:corebluetooth`, `unified-ble:winrt` and `unified-ble:bluez-dbus` (`COREBLUETOOTH_BACKEND_ID`, `WINRT_BACKEND_ID`, `BLUEZ_BACKEND_ID`), and adapter ids `corebluetooth-default-adapter`, the BlueZ object path (`/org/bluez/hci0`) and the raw Windows adapter device id. An adapter id depends only on that adapter, so a persisted `adapterId` keeps working when other adapters come and go.

If there is no adapter, the factory throws `adapter.unavailable`. If more than one adapter exists and you omit `adapterId`, the factory picks the first one in a deterministic order (by adapter id), so a single-adapter host needs no configuration and a multi-adapter host selects the same controller every run. `provider.listAdapters()` lists every OS adapter. An adapter the OS could not describe is still listed, as `unavailable` with the OS's reason. Pass `adapterId` to target a specific controller.

On `SIGINT`/`SIGTERM`, await `manager.destroy()`. Then scan/connect/GATT with the same `BleManager` helpers as React Native.

### Load and identity failures

The addon is found only from the installed package's own location: never through the process cwd, never from another platform's or architecture's binary, and never through a TypeScript fallback. Before any radio call, the host checks the binary's `nativeBuildIdentity()` against the identity the package was sealed with: contract revision, source digest, binding schema, target, and release profile.

| Cause                                                               | Error                                                                                       |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| No prebuild for this platform/arch/libc                             | `capability.unavailable` · `<host>.native-boundary.load` · `no-prebuilt-for-target`         |
| The OS refused to load the file                                     | `capability.unavailable` · `<host>.native-boundary.load` · `load-failed` (dlopen text kept) |
| File does not match its identity sidecar, or the sidecar is missing | `protocol.incompatible` · `<host>.native-boundary.version`                                  |
| Binary identity differs from the package                            | `protocol.incompatible` · `<host>.native-boundary.version` (the differing fields are named) |
| `UBM_NAPI_ADDON` is not an absolute path                            | `argument.invalid` · `<host>.native-boundary.load`                                          |
| Factory called on the wrong OS                                      | `capability.unavailable` · `<host>.native-boundary.load` · `{linux,macos,windows}-required` |

`<host>` is the 4.x operation prefix of each OS: `direct-gatt` (CoreBluetooth), `winrt` or `bluez`.

### Scan observations

The core queues reports only while a scan is live and clears the queue when a scan starts or stops, and the provider refuses (with a `scan-observation-foreign` diagnostic) any observation the core queued for another scan. A report is not necessarily a fresh over-air advertisement: scan admission can reobserve known OS device state. Each native observation's `receivedAtMonotonicMs` is when the core received it, on the host's clock, not when the host took it. `provenance` distinguishes `platform-raw`, `platform-derived`, and `core-merged`. The OS's merged device state (BlueZ `Device1`, a known-device report) is `platform-derived`; a single advertisement's own data is `platform-raw` on WinRT and `platform-derived` on CoreBluetooth (a parsed advertisement dictionary) and BlueZ.

Public scan observations preserve this provenance and the optional exact `origin`:
`advertisement` or `device-state`. The latter may contain the OS's cached service
UUID union, including Classic services; it is not a claim that those UUIDs were
advertised in one BLE packet. Electron and Tauri preserve supplied origin through
IPC. Either metadata field remains absent when the producer does not supply it;
clients must not infer origin from RSSI, platform, or service UUIDs.

### System-connected peers on macOS

A peripheral already connected elsewhere may no longer advertise. Use
`manager.peers.connected({ services: ['180d'], timeoutMs: 10_000 })` to retrieve
matching CoreBluetooth peers, then `manager.connect(peer)` to acquire your own
connection lease. Lookup does not connect, disconnect, or claim ownership. The
service filter is mandatory and uses OR semantics. Persisted application-scoped
references can be resolved without scanning; identifier resolution alone reports
connection state as unknown. See [peer directories](PEERS.md#corebluetooth-desktop-retrieval)
for query limitations, cancellation and the distinction between OS connection
membership and local ownership.

### Error operation ids

Every public error reports the 4.x operation id of its host and operation, as the TypeScript backends did: `direct-gatt.connect`, `winrt.gatt.read`, `bluez.scan.start`, `direct-gatt.gatt.database-read` (a database handle's read on CoreBluetooth and WinRT; BlueZ handles reported `bluez.gatt.read`), `winrt.security.pair`, `bluez.provider.select-adapter`, and so on. The core's own operation for the failure (for example `gatt.read` or `discovery.database-bound`) never becomes the public id. When the error has no OS detail it is kept for diagnosis in `platform.metadata.coreOperation` of the `desktop-rust-core` / `core-detail` detail.

Source mode, for contributors only: `UBM_NAPI_ADDON=/absolute/path/to/ubm_echo.<platform>-<arch>.node` loads that checkout build exclusively (built by `node scripts/ci/build-napi-addon.js`). Digests are still checked. Only the release-profile rule is relaxed.

### BlueZ connection policy, bus and pairing generation

`createBluezBleManager`, `createBluezProcessHost`, `createDbusNextBluezBackendProvider` and Electron main's `createElectronMainBluezBackendProvider` share these trusted host options.

The shared Rust authority resolves and pins the current unique D-Bus owner of
`org.bluez` itself. Applications do not copy daemon owner strings. Optional
`connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner }` restricts construction
to a deliberately supplied owner; it is not an implementation attestation.
An introspection entry alone is not proof: BlueZ can expose an unimplemented
LE interface. The selected adapter must answer the versioned
`LinuxAuthority1.GetContract` lease/GATT handshake `(1,2,1)`. Lease revision 2
includes the actual, exact-generation MGMT disconnect observation in a physical
release answer; older daemon revisions are refused rather than losing that
detail when the release reply precedes the event. The backend checks
the owner pin before connection effects and never substitutes a restarted daemon or generic
device-wide `Device1.Connect`/`Disconnect`. It does not run privileged commands,
modify daemon configuration, or add a compatibility fallback.

The native release answer is also bound to the caller's original peer, lease and
public connection generation. Its observed platform detail reaches the public
lifecycle terminal directly from that answer, without waiting for event delivery.
The public cleanup receipt remains `state` and `failures`; an unobserved reason
remains absent, and a refused release retains ownership for retry.

Migration from earlier candidates: omission selects native owner binding, not
scan-only admission. Actual LE lifecycle and strict discovery support still
require the corresponding implemented daemon mechanisms; automatic owner
binding does not create those mechanisms or claim readiness. A retired manager
never transfers its leases to a replacement daemon. Construct a fresh manager
to resolve the replacement owner after the old ownership is settled. There is no
legacy policy mode. The same policy applies to a process host's borrowed managers
and native continuation because they share its central. Policy is BlueZ-only;
CoreBluetooth and WinRT reject it.

Accepted token-bound LE connect/release replies remain owned when a caller cancels.
An exact terminal release observation may remain as one peer/generation fact
after native obligations reach zero, so an original cancelled or concurrent
waiter can consume its own answer. The matching public transition consumes it;
newer peer admission supersedes it. This retained fact is not a live native lease
or ACK maintenance debt, and it cannot supply a reason for a newer connection.
An indeterminate reply is not permission to resend the effect or acquire a new
generation; a refused release stays retryable. Resolving an unknown address uses
separately owned, adapter-scoped LE discovery, never `ConnectDevice`. Its accepted
start/stop replies also survive cancellation. A refused discovery stop remains
cleanup debt and is reported by transport close rather than logged as success.
All resulting peer identities remain fenced to the original daemon owner.

### Linux initial deferred acquisition

Maintained daemon `5.87-ubm.5` adds optional observer revision 1 on
`org.unifiedblemanager.LinuxAuthority1`: `GetLeAvailability` reports the current
monotonic advertisement sequence, and `LeAdvertisement` reports fresh
connectable LE advertisements with their device and sequence. The client
registers its owner-fenced observer before reading the sequence baseline, then
owns a separate LE discovery session. Only a matching fresh report admits the
existing token-bound connection route. Cached `Device1` objects, RSSI changes,
Classic reports and scan-response-only reports do not establish availability;
stock merged discovery observations remain bearer-ambiguous.

The instantiated backend probes this optional mechanism; older daemons retain
direct acquisition but report `capability.unsupported` for `when-available`.
Cancellation and the original deadline retain discovery cleanup obligations;
one peer's discovery sender cannot stop another peer's or a public scan's
session. Installing or replacing the daemon remains an explicit privileged host
decision, not an operation performed by the package. The authority tuple remains
`(1, 2, 1)`. Producer and private-bus validation are not physical qualification;
the retained `.4` physical receipts do not qualify this new observer.

LE lifecycle support does **not** establish authoritative GATT discovery
readiness. Stock BlueZ's aggregate `ServicesResolved`, exported service objects,
and MTU are insufficient to prove successful, current LE-specific discovery.
The strict discovery route additionally requires the version-1
`org.unifiedblemanager.LEGatt1.GetSnapshot` extension. It registers observation
before admission and brackets the entire graph with the same accepted ready
identity. Missing/unknown API is `capability.unsupported`, not readiness inferred
from cached objects. The bundled extension and its explicit preparation/deployment
requirements are described in [BlueZ LE GATT](BLUEZ_LE_GATT.md). Isolated build and
private-bus evidence do not establish physical-radio qualification.

Capabilities come from that instantiated central. Without LE authority,
`connection:direct` and its dependent `background:desktop-maintain-connection`
report `unsupported` with `bluez-le-bearer-attestation-required`; a static
platform table never overrides that refusal. Native continuation cannot bypass
the same connection boundary. Deterministic radios and other host backends keep
their own capability answers.

`busKind` reaches the core as the central's D-Bus bus. `'system'` is the default. `'session'` serves a BlueZ exported on the session bus (mock or sandboxed daemons). A build that cannot reach that bus answers `capability.unsupported`; it never falls back silently to the system bus. Any other value is `argument.invalid`.

`pairingGeneration` (a host-supplied `BluezPairingGenerationController`) is carried to the core. A pair that directs `secureConnections: 'require' | 'disallow'` then holds the adapter-wide generation for the ceremony, and the core restores the previous value afterwards. Without a controller, a directed generation is `capability.unsupported`. The package never escalates on its own. While the generation is held, the setting applies to every pairing on that adapter.

### Adapter state, admission and adapter loss

The adapter snapshot reports what the OS says, in the 4.x vocabulary. CoreBluetooth `resetting` is power `resetting`. `unsupported` is availability and power `unsupported`, with authorization `unavailable`. `unauthorized` is authorization `denied`.

A CoreBluetooth central waits up to 10 s for its first usable state (powered on, not refused) before listing or creating succeeds. Otherwise it fails with `capability.unavailable`, platform code `adapter-initialization-timed-out`, as 4.x did.

Radio work is refused before any radio effect, with the 4.x per-OS admission:

- **CoreBluetooth** checks authorization first (`permission.denied`, `permission.restricted`, `permission.not-determined`), then availability (`adapter.unavailable`), then power (`adapter.powered-off`, `adapter.resetting`).
- **WinRT** checks availability first, then authorization, then power.
- **BlueZ** additionally verifies the pinned daemon's implemented Linux authority
  before connection/GATT admission. Scanning does not claim that capability.

Adapter loss invalidates affected connection, database and subscription generations;
it does not authorize guessing that an outstanding native release succeeded.
BlueZ owner replacement retires the original authority and fails closed. Old
tokens retain their original owner/device/generation, and cleanup never routes
them to a replacement daemon. A fresh manager resolves the replacement owner
and verifies its contract; old peer handles are not rebinding credentials.
Physical LE loss carries its authenticated generation and actual MGMT reason,
so a buffered old event cannot invalidate a newer connection. An ATT/database
invalidation is not proof of physical ACL termination.

The core publishes lifecycle, scan-end, security, write-readiness and adapter-reset events on bounded queues (256 each). A backend that falls behind is told how many it missed, and re-reads the core's own state rather than guessing: every live link's connection state and generation, and its database state (`connection-lost`, `database-changed`, or the adapter-loss sequence above while the adapter is lost); which scan the core still owns (a scan it no longer owns ends `source-failed`); and each watched peer's security state and write readiness. A link the core still reports live and current gets no event.

A subscription's `delivery.overflowPolicy` governs the core's buffer for that consumer too, and values the radio lost before the core could hold them count against it. With `error`, such a loss ends the stream with an `overflow` terminal carrying the counts, as a local overflow does. With `drop-oldest`, `drop-newest` or `latest`, the stream reports an overflow notice with the cumulative counts and keeps delivering.

A notification value the binding hands over malformed ends that subscription's stream `source-failed` with `protocol.malformed`; it is never dropped.

Delivery is event-driven, as the 4.x native callbacks were. The addon wakes the provider as soon as it queues an advertisement, a notification value, or a lifecycle, adapter, reset, security, write-readiness or scan-end report. The provider's polls (5 ms for scans and notifications, 10 ms for the other core events) remain only as a safety net. When a link is lost, the database changes or the adapter is lost, every value the core still holds for a subscription is delivered in order before that stream's terminal.

A core error that carries the OS's own answer reports it as the 4.x platform identity: CoreBluetooth `{ domain: 'corebluetooth', code: <NSError code> }`, WinRT `{ domain: 'winrt', code, metadata: { hresult, gattStatus } }`, BlueZ `{ domain: 'bluez-dbus', code: <D-Bus error name> }`, with the OS message as `safeMessage`. An error with no OS answer carries `{ domain: 'desktop-rust-core', code: 'core-detail' }`.

### Option audit

The table distinguishes options forwarded to their native/core mechanism from
options rejected locally before dispatch. The option-audit tests assert zero
dispatch for refusal rows; provider/core regressions verify actual routing and
ownership for supported rows.

| Option                                                   | Behaviour                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Scan query / filters                                     | Required service UUIDs go to the OS scan filter (`scanner.plan`). A caller's `localNamePrefix` also goes to the OS: BlueZ receives it as the `SetDiscoveryFilter` `Pattern`, as the 4.x dbus-next backend sent it; CoreBluetooth and WinRT have no OS name filter. `Pattern` also matches an address prefix, so the OS only narrows. Name-prefix, manufacturer and address predicates always match in software as the final filter.                                                                                                                                 |
| Scan `duplicatePolicy`                                   | Carried to the OS scan: `all` asks for every advertisement; `first` and `merged` ask the OS to filter repeats (BlueZ `DuplicateData: false`, CoreBluetooth `AllowDuplicates: NO`). `first` also delivers one sighting per peer per consumer. `merged` is the default of `scanForServices` / `scanUntil`.                                                                                                                                                                                                                                                            |
| Scan `platform` options                                  | `capability.unsupported` (not registered)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Scan share / join                                        | One core scan fanned out to joined leases. A forged token is `ownership.denied`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `deliveryMode` `require-*`                               | Checked against the characteristic's properties before any dispatch (`gatt.property-not-supported`). The delivery a value reports is what the core observed.                                                                                                                                                                                                                                                                                                                                                                                                        |
| Write without response                                   | Resolves `commitState: 'unknown'` (never `confirmed`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Descriptor write without response                        | `capability.unsupported`, as the legacy WinRT addon answered: descriptors are written with a response                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Connection `intent: 'when-available'`                    | macOS uses a pending CoreBluetooth request; Windows acquires `GattSession.MaintainConnection` before GATT discovery and awaits actual `ConnectionStatusChanged`. Linux uses the optional maintained-daemon LE observer described below before token-bound acquisition. All retain the original deadline/AbortSignal and scoped cleanup. These are initial-acquisition mechanisms, not automatic post-loss reconnect. Older Linux daemons without the observer report `capability.unsupported`; cached records and stock merged discovery signals cannot substitute. |
| Connection `preferredPhy`                                | `capability.unsupported`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Connection `transport` other than `le`/`auto`            | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `bluezBus` / `pairingGeneration` on a non-BlueZ provider | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| Unknown `busKind` (BlueZ)                                | `argument.invalid`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `restoration`                                            | refused by the Node host                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |

Cancellation: an aborted `AbortSignal` cancels exactly the in-flight core operation, through a ticket the host mints before the call. An abort before admission ends the operation with `operation.aborted` and no radio call. A caller deadline crosses to the core as a relative budget. Connection acquisition without a deadline is unbounded for both intents: it waits for the native result or cancellation; other operations retain their documented liveness backstops.

### Parity with the 4.x desktop backends

Every capability the TypeScript CoreBluetooth, WinRT and dbus-next BlueZ backends offered is tracked in `DESKTOP_RUST_CORE_PARITY`, which `unified-ble-manager/testing` exports (no production entrypoint exports it). Runtime registration reads the instantiated central's `runtimeCapabilityStates`, not the static OS diagnostic table, and is narrowed to mechanisms this provider wires. The native snapshot accepts all four canonical states. Connection refusals retain `unavailable` versus `unsupported` and the instance's reason; a native `supported` mechanism does not by itself promote the public provider's deterministic evidence label.

**Implemented on the Rust path:**

- Scanning: the OS service-UUID scan filter; software name, manufacturer and address filters; scan share/join; first-sighting duplicates.
- Events: `connection-lost` and `database-changed`.
- Adapter: power, authorization (macOS, Windows), watch, and enumeration/selection.
- Operations: exact in-flight cancellation; an honest without-response commit state; the `require-*` delivery check. WinRT additionally carries the requirement to the OS and prefers notify over indicate.
- Connection: CoreBluetooth connected RSSI; maximum write length and long write; Windows maintain-connection.
- Advertisements: solicited and overflow UUIDs and `connectable` (macOS).
- Security (Windows, Linux): state, pair, cancel, unpair and security events, plus the BlueZ pairing-generation controller.
- Linux: address targeting, advertisement address type, extended characteristic flags and access requirements, and the priority/parameter reasons.

- macOS: the write-without-response readiness watch (`DesktopCentral::write_readiness`). It is registered only where the core reports a readiness signal.
- Windows: the scan-terminated event, from the core's scan source-closed terminal.
- Linux: adapter listing and selection on the session bus (`list_adapters_on`).

- LEGACY-AUDIT-1:
  - the adapter-loss teardown and generation advance;
  - admission errors;
  - the CoreBluetooth first-state wait;
  - `resetting` / `unsupported` states;
  - `merged` scans that reach the OS duplicate filter, and LE-only BlueZ scans;
  - repeated service, characteristic and descriptor UUIDs that keep their instances;
  - uncached WinRT discovery;
  - maximum write length without discovery;
  - WinRT selection of any listed adapter, with its `deployment`;
  - the 4.x backend and adapter ids;
  - the `unsupported` rows that keep their 4.x reasons.

  The rows with kept reasons are CoreBluetooth `connection:request-mtu` (`corebluetooth-auto-negotiated-mtu`), `connection:effective-mtu` and `connection:phy`, and BlueZ `security:pairing-generation`. The BlueZ row carries the privilege explanation without a host controller, and the adapter-wide blast radius with one. The vendored btleplug patches the loaded core links are reported as `diagnostics.btleplugPatches`.

**Open release blockers:** none on the TypeScript/N-API path. `__tests__/backends/desktop/desktop-parity-blockers.test.js` fails if a row is ever marked `blocked` without a probe. The deterministic synthetic radio does not prove physical-radio behaviour. The physical checks listed under [Verification](#verification) are still outstanding.

## Native continuation in a trusted process host

The explicit `createCoreBluetoothProcessHost`, `createWinRtProcessHost`, and
`createBluezProcessHost` factories select one adapter and retain one native
central/backend for the process lifetime. `host.createManager()` returns an
ordinary public manager borrowing that owner; destroying it releases its own
resources, not another manager or the native continuation. Trusted main-process
routers can use `host.createInternalManager()` with the same ownership authority.
The ordinary one-call manager factories keep their existing lifetime semantics.

Use `host.continuation` for native execution and explicit backlog claims.
`host.continuationAccess` is the narrow canonical-envelope bridge for an
authenticated renderer: preparation and acknowledgement remain separate.
Host destruction immediately seals new manager/execute/storage-configuration
admission, revokes borrowing managers, and closes the native owner. Failed
cleanup retains the same host for retry. Status and explicit claims remain
available after closure; destroying the host does not implicitly consume a
volatile backlog or acknowledge/delete a durable journal. A confirmed parent
release permits exact retained child cleanup without reopening the radio.

If initialization fails while cleanup is still owned,
`DesktopProcessHostInitializationError` preserves `originalCause`,
`cleanupCause`, and `retryCleanup()`. Retain and retry that handle until its
receipt is `released`; it never opens a replacement radio. An ordinary
initialization error retains no JavaScript process-host/central cleanup handle:
it either precedes that allocation or follows confirmed compensation. Native
radio-open failures still follow the platform's documented partial-open cleanup
policy; this is not a claim that all OS bookkeeping has disappeared.
Offline recording retrieval continues to use the separate existing recording
store factory, without creating a process host or opening BLE.

For an authenticated application bridge, `createNativeContinuationControl(access)`
provides the same `execute`, `status`, and `claim` decoding without exposing a
central or filesystem configuration. The access supplies `execute(peerId,
declarationJson)`, `describeBacklog()`, `prepareClaim(maxItems, maxBytes)`, and
`acknowledgeClaim(token)`, returning the existing canonical native envelopes.
It is also exported from `/electron/renderer` and `/tauri`; those entrypoints
export `createNativeContinuationRecordingController` for ID-only offline journal
controls. These helpers do not install a transport or authenticate its caller:
the trusted host must enforce sender authorization and response bounds.

Run the claim helper on the receiving side. It decodes the prepared bytes
before acknowledging, never acknowledges a malformed or undelivered prepared
response, and retains decoded values with `disposed: false` if acknowledgement
is uncertain. Do not replace this handshake with a main-process `claim()` call
whose already-acknowledged result can be lost when a renderer disappears.

For a recorder that must keep collecting while its UI is absent, the trusted
host can attach a native continuation controller to its **already-open** Rust
central. Node/BlueZ, Node/CoreBluetooth, Node/WinRT and Electron main export the
same controller. It never opens another radio. `loadDesktopCoreBinding` verifies
the packaged addon's build identity before the host opens its central.

Pass the exact peer ID reported by the selected radio. Native identity parsers
enforce their canonical spelling before connection admission (for example,
lowercase CoreBluetooth UUIDs). A noncanonical alias fails with
`argument.invalid` and the canonical identity in its detail, before a lease or
physical connection is created. Opaque radio IDs are not globally case-folded.

```ts
import { loadDesktopCoreBinding, createNativeContinuationController } from 'unified-ble-manager/node/corebluetooth'

const binding = await loadDesktopCoreBinding({
  platform: 'corebluetooth',
  operationPrefix: 'direct-gatt'
})
const central = await binding.openProduction({
  owner: 'application-native-recorder',
  platform: 'corebluetooth',
  adapterId: null
})
const continuation = createNativeContinuationController(central)
await continuation.execute({
  onAppearance: 'native',
  // Use the observed peer ID unchanged: Apple UUID, WinRT MAC, or BlueZ
  // adapter-scoped ID such as hci1/dev_AA_BB_CC_DD_EE_FF.
  peerId: '11111111-2222-3333-4444-555555555555',
  resubscribe: [
    {
      serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
      serviceOccurrence: 1,
      characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
      characteristicOccurrence: 1
    }
  ]
})

// Call later, when the application is ready to take over these buffered values.
async function finishRecording() {
  const backlog = await continuation.claim({ maxItems: 256, maxBytes: 65536 })
  console.info(backlog.values, backlog.streamEnds, backlog.afterCutoffLoss)
  if (!backlog.disposed) throw new Error(backlog.disposeFailure ?? 'Native cleanup is still owned')
  return backlog
}

// At application shutdown, after every other user of this central has finished:
async function shutdownRecorderHost() {
  const cleanup = await central.close()
  if (cleanup.state !== 'released') throw new Error('Central cleanup needs a retry')
}
```

For BlueZ or WinRT, import their explicit Node entrypoint and pass the matching
`platform` (`bluez` or `winrt`) and operation prefix. Reuse the central your
custom host already owns; do not open this recorder beside a second manager
for the same radio.

`status()` exposes the bounded queued-data count, the last collection error and
the native recovery outcome. Recovery runs in Rust without a JavaScript pump,
using the same declared selectors and authoritative retryability as mobile.
`claim()` seals intake, strictly decodes the prepared backlog, then acknowledges
the handoff; it ends this recording generation. An uncertain acknowledgement
returns the decoded values with `disposed: false`, never hides them in a rejected
promise. Preserve those values and retry cleanup. Report control loss, overflow
terminals and after-cutoff loss; a buffer is not a promise of lossless recording.

The default backlog is bounded process memory, **not durable storage**. For
opt-in durable collection, first call `continuation.recordings(privateDirectory)`
on this same central, then include `recording: { id, maxBytes, maxRecords }`
in the native declaration. The trusted host chooses its private directory;
never forward an arbitrary renderer path. Data records go to the bounded
journal instead of the volatile data queue, and native claims retain a recording
ID without reading or acknowledging that independent journal.

The returned recording controller exposes `status`, `prepare`, `acknowledge`,
`stop` and `clear`. Save or process a prepared batch before acknowledging its
exact token. `stop` ends recording admission, not radio ownership; its receipt
says `radioRelease: 'not-requested'`. After central cleanup or a process restart,
`openNativeContinuationRecordings(binding, privateDirectory)` from the same
explicit Node entrypoint opens the journal without enumerating or creating a
radio. It verifies the native binding identity as usual. The journal is plaintext
(`encrypted: false`); quotas and storage failures are explicit. See the
[durable storage contract](BACKGROUND.md) for bounds, protection and failure
semantics. Generic declared `setup` steps can restore an application protocol
after resubscription; UBM itself contains no Polar command recipe.

Keep the owning process and central alive while collecting. OS service registration, process
relaunch and persistence of the standing declaration remain explicit host
integration; this API does not install a daemon, elevate privileges or promise
collection after process death. Ordinary one-call manager factories therefore
still refuse non-default `background.continuation` options: they have no
configured native process-lifetime owner. Package SemVer does not qualify
untested radios; see [background execution](BACKGROUND.md).

## Advanced provider construction

> **Maintainer/host-authoring reference — not ordinary application construction.**
> Use the one-call factories above for application code. This provider example
> is for maintainers implementing a host integration or authors wiring an
> explicitly selected backend; it is not the normal application recipe.

```ts
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } from 'unified-ble-manager/advanced'
import {
  coreBluetoothCompatibility,
  createNativeCoreBluetoothBackendProvider
} from 'unified-ble-manager/node/corebluetooth'

const now = () => performance.now()
const provider = createNativeCoreBluetoothBackendProvider({ now })
const adapters = await provider.listAdapters()
if (adapters[0] === undefined) {
  throw new Error('No CoreBluetooth adapter is available.')
}

const manager = await createBleManagerFromProvider(
  {
    provider,
    selection: { selectedAdapterId: adapters[0].adapterId },
    coreCompatibility: coreBluetoothCompatibility,
    manager: {
      clientId: 'node-corebluetooth-client',
      managerId: 'node-corebluetooth-manager',
      ownerMode: 'owning'
    }
  },
  { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
)
```

WinRT has the same shape, with `createNativeWinRtBackendProvider` and `winRtCompatibility` from `unified-ble-manager/node/winrt`. BlueZ:

```ts
import { createDbusNextBluezBackendProvider } from 'unified-ble-manager/node/bluez'

const provider = createDbusNextBluezBackendProvider({ busKind: 'system', now })
```

The name `createDbusNextBluezBackendProvider` is historical: it returns the shared Rust core provider, and no dbus-next transport is involved. All three are `createDesktopRustCoreBackendProvider({ platform, owner, now })` underneath, which every desktop entrypoint also exports. Deterministic suites use `createTestDesktopRustCoreBackendProvider` from `unified-ble-manager/testing` instead, which additionally accepts the synthetic radio and the binding, platform and first-state-timeout seams.

Then scan and GATT through the same `BleManager` as React Native. Await `manager.destroy()` when the process session ends. Its cleanup record reports every release failure the core recorded.

Failed cleanup does not reopen admission or discard native ownership. Retain the owner and retry teardown when its receipt reports incomplete release. Desktop shutdown retains unresolved scan, subscription and event-transport obligations; a later confirmed release clears current cleanup debt without erasing earlier diagnostics.

WinRT retries only unconfirmed handler, watcher and maintained-session cleanup stages. Additional leases reuse a healthy maintained session. If opening a watcher fails and compensating cleanup is also refused, a process-owned cleanup vault retains that partial watcher for the next native radio open or explicit close on the same adapter. An unrelated adapter does not inherit that cleanup debt. There is no background retry loop; without either trigger, the retained owner lasts until process exit. These ownership rules do not constitute Windows physical-radio qualification.

## Verification

Hardware-free (any host): `pnpm test:package` drives the provider through the real addon on its deterministic synthetic radio. Every verb executes in Rust. The first-party TCK legs that `unified-ble-manager/testing` exports (`createCoreBluetoothFirstPartyTckRegistration`, `createBluezFirstPartyTckRegistration`, `createWinRtFirstPartyTckRegistration`) run the same way: the `/testing` provider (`createTestDesktopRustCoreBackendProvider`) over the addon's synthetic radio, never a production open. They are deterministic proof only.

Clean packed consumer, run from the checkout:

```sh
pnpm native-prebuild:build -- --backend desktop-core   # this host's release prebuild + sidecar
pnpm prepack && npm pack --pack-destination /tmp/ubm-pack
node scripts/ci/napi-clean-tarball-acceptance.js --tarball /tmp/ubm-pack/unified-ble-manager-*.tgz --pm npm --negative
node scripts/ci/napi-clean-tarball-acceptance.js --tarball /tmp/ubm-pack/unified-ble-manager-*.tgz --pm pnpm --negative
```

The default `--probe identity` stops before any radio open. It loads the prebuild from the installed package, verifies identity, and drives a synthetic scan/connect/discover/subscribe/notify round trip in Rust. It is safe in a process without Bluetooth permission. `--probe radio` calls this OS's public no-options factory on the real adapter. Run it from a Bluetooth-authorized terminal on macOS: a headless host passes with `adapter.unavailable`.

Physical checks still outstanding: a peripheral session per OS (scan, connect, discover, notify, write, disconnect); pairing on Windows and Linux, including the BlueZ pairing-generation controller; the Windows scan-terminated event when the radio is turned off; and a session-bus BlueZ under `dbus-run-session`.

## Electron

Do not load a Node radio factory from a renderer. See [`ELECTRON.md`](ELECTRON.md).

## Maintainers

[Current 5.0 authority](README.md#current-50-authority), [`PLATFORMS.md`](PLATFORMS.md).
