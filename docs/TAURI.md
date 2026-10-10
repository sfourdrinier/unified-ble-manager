<!-- docs/TAURI.md -->

# Tauri v2

Tauri webviews use the zero-plumbing `createTauriBleManager()` factory, which returns the public `BleManager`. The test-only `createTauriBleManagerWithEnvironment(...)` entrypoint accepts injected `invoke` and `Channel` implementations.

The Rust plugin owns the radio (btleplug: CoreBluetooth, WinRT, or BlueZ). The webview never loads a Node addon.

After an adapter reset, commands from the ended attachment report `backend.reset`
before resolving GATT handles or reserving native work. This also applies when
the lifecycle event invalidates the database before the caller is rebound.
Release commands remain available to settle retained ownership. Non-GATT
operations are tracked before waiting for authority admission so cancellation
can settle them without dispatching native I/O.

## Install

```sh
pnpm add unified-ble-manager@5.0.3 @tauri-apps/api
```

Use the Rust plugin source shipped in the same npm package. In the normal
Tauri layout (`src-tauri/` beside `node_modules/`), put the following entries
in the consuming app's `src-tauri/Cargo.toml`. `ubm init --host tauri --dir
src-tauri` generates this fragment from the same recipe used by the external
packed-consumer build:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }

# Cargo reads patches only from the consuming workspace root.
[patch.crates-io]
btleplug = { path = "../node_modules/unified-ble-manager/vendor/btleplug" }
bluez-async = { path = "../node_modules/unified-ble-manager/vendor/bluez-async" }
```

Adjust all three relative paths together in a monorepo. Omitting the root
`[patch.crates-io]` table fails the plugin's production vendor-patch guard;
Cargo intentionally ignores patch tables in dependency manifests.

The crate is not yet published on crates.io. Use the packed npm path recipe
until a separately published crate exists.

## Frontend

```ts
import { createTauriBleManager } from 'unified-ble-manager/tauri'

// A resolved release-failed receipt is a failure, not a successful cleanup.
async function withCleanup<T>(
  work: () => Promise<T>,
  cleanup: () => Promise<{ readonly state: 'released' | 'release-failed'; readonly failures: readonly unknown[] }>,
  label: string
): Promise<T> {
  let outcome: { kind: 'value'; value: T } | { kind: 'error'; error: unknown }
  try {
    outcome = { kind: 'value', value: await work() }
  } catch (error) {
    outcome = { kind: 'error', error }
  }
  try {
    const receipt = await cleanup()
    if (receipt.state === 'release-failed') {
      throw new Error(`${label} failed`, { cause: receipt })
    }
  } catch (cleanupError) {
    if (outcome.kind === 'error') {
      throw new AggregateError([outcome.error, cleanupError], `${label} and operation failed`)
    }
    throw cleanupError
  }
  if (outcome.kind === 'error') throw outcome.error
  return outcome.value
}

const abort = new AbortController()
const manager = await createTauriBleManager()
await withCleanup(
  async () => {
    const scan = await manager.scan({
      query: { anyOf: [{ services: { any: ['180d'] } }] },
      duplicates: 'coalesced',
      delivery: 'balanced',
      signal: abort.signal,
      timeoutMs: 15_000
    })
    const peer = await withCleanup(
      async () => {
        const first = await scan.observations[Symbol.asyncIterator]().next()
        if (first.done || first.value.kind !== 'value') throw new Error('No peer observed')
        return first.value.value.peer
      },
      () => scan.stop(),
      'scan stop'
    )
    const connection = await manager.connect(peer, { signal: abort.signal, timeoutMs: 10_000 })
    await withCleanup(
      async () => {
        const gatt = await connection.discover({ signal: abort.signal, timeoutMs: 10_000 })
        // Public UUIDs are canonical 128-bit values; lookup accepts short forms.
        const level = gatt.characteristic('180f', '2a19')
        const bytes = await level.read({ signal: abort.signal, timeoutMs: 5_000 })
        void bytes
      },
      () => connection.release(),
      'connection release'
    )
  },
  () => manager.destroy(),
  'manager destroy'
)
```

`BleManager.scan` accepts the frozen `ScanQuery` Boolean algebra plus `signal`, `timeoutMs`, duplicate policy, and a stream preset. Query matching is performed by the shared portable matcher; native projections are only safe broad prefilters.

Tauri uses its native attachment identity. Its factory accepts an adapter
selector and rejects `instanceId`, `diagnostics`, `randomBytes`, `restoration`,
and `background` declarations with a typed capability error; an unsupported
background standing order never silently becomes record-only.

**Scan-query digest.** The webview normalizes the query and sends its `scan-query-v1:` digest (FNV-1a/64 over the canonical JSON) with the request; the plugin recomputes the digest over its own canonicalization and fails closed with `protocol.malformed` at `tauri.scan-query` on any mismatch, before any radio work. The TypeScript normalizer owns the canonical form — null `services`/`names`/`manufacturerData`/`serviceData`/`rssi` are kept, null `peers`/`addresses` are dropped — and the plugin reproduces it byte-identically, including radio `addresses`, UUID case/short forms, and explicit wire nulls. The shared golden corpus `__tests__/fixtures/scan-query-digests.json` (generated by `scripts/generate-scan-query-digest-corpus.js`) pins all supported shapes on both sides.

Remote streams preserve bounded delivery and overflow notices. GATT objects are immutable, generation-bound views.

Each renderer lease can own up to 256 readiness watches and 256 parameter
watches concurrently. Closing a watch frees its live capacity immediately.
The plugin separately retains the most recent 256 successful close handles per
watch type so repeated close requests are idempotent within that history. Older
unknown close handles fail `ownership.denied`; they never identify a current
watch. Renderer clients mint a fresh handle for every acquisition and do not
reuse a released handle. Lease retirement clears that lease's ownership scope.

**Discovery.** `gatt.discover` renders one characteristic record per characteristic — descriptor-level core rows repeat their characteristic's identity, so they never mint a second record — with the core's own occurrence numerals, the same grouping the Node desktop path applies; duplicate-UUID characteristics keep distinct per-UUID occurrences on every host. A rediscovery replaces the snapshot: the previous database goes stale (`gatt.stale-handle`). A snapshot that still violates the topology (duplicate service/characteristic/descriptor paths, orphan parents) fails with `protocol.violation` naming the offending path — uuids and occurrences — in the error's `platform` detail (`domain: 'gatt'`), never only a bare code.

## How the plugin runs operations

The frontend shares Electron's [connection and GATT recovery contract](ELECTRON.md#connection-and-gatt-recovery):
invalidation is immediate, cleanup ownership survives failure, and bounded
child cleanup cannot prevent the scoped parent-release request. Local iterator
cleanup remains distinct from confirmed native release.

**IPC protocol version.** The webview and the plugin speak IPC protocol 6 and
each offers exactly that version. Version 5 adds security routes, address
targeting, connection intent and platform scan-option forwarding while retaining
relative `budgetMs` deadlines, error `commit`, subscription `delivery`, connection
lifecycle events and attachment rebind. The instantiated native authority still
determines which mechanisms are available. An older protocol 4 host might ignore
new option fields, so protocol 4 and earlier peers are refused at bootstrap with
`protocol.incompatible` before any operation runs, whichever side is older.
Upgrade the npm package and crate together.
`TAURI_PLUGIN_COMPATIBILITY.ipcProtocol` reports the required version.

On eligible Linux characteristics, `acquireWrite()` and `acquireNotifications()`
use the native central's optional BlueZ acquired transports. The webview sees
opaque owner-scoped handles, the returned MTU and copied packets. It never
receives an OS descriptor. Acquisition, backpressure, cancellation and cleanup
follow the [acquired transport contract](NODE.md#explicit-bluez-acquired-gatt-transports).
A failed close retains its exact native lease for retry. Rediscovery, window
teardown and connection release retire the acquired children. Availability
comes from the instantiated native capability and the characteristic's
eligibility; IPC support does not establish physical-radio qualification.

Connection-parameter watch acquisition prefers native events received during
its initial probe over that delayed probe's answer. A native queue gap causes
a live parameter re-read; failure ends the stream with that failure's original
detail, rather than substituting zeros or stale cached parameters.
The initial probe and any opening-time reconciliation keep the caller's original
absolute deadline and cancellation scope. Each native read uses a child ticket;
completing one read does not settle the still-opening watch request.

The plugin owns one shared Rust central (`ubm-desktop`) and never serializes BLE work behind a lock of its own: a slow connect or discovery on one peer does not delay another peer's notifications, a cancel, or shutdown. The central and its radio open once, on the shared desktop executor, the first time a BLE operation needs them.

- **Deadlines.** The webview sends the time left on the caller's deadline as a relative `budgetMs` (a non-negative safe integer, or absent when the caller gave none); its own clock never crosses the boundary. The plugin starts counting when the request arrives, so time spent queued in the plugin counts against the budget. Any other `budgetMs` fails with `protocol.malformed` before any effect. Without a budget, liveness backstops bound the operation (120 s for discovery and GATT; 10 s for scan stop, unsubscribe and disconnect; 30 s for starting an OS scan); a backstop expiry is `operation.timed-out` with the detail `liveness-backstop`. A connect without a budget has no backstop: it waits as long as the OS does, as Tauri 4.x did, and a cancel ends it.
- **Cancellation.** `operation.cancel` reaches exactly the one core operation behind the correlation, including one that has not reached the core yet (it then ends `operation.aborted` without a radio call). The cancelled request reports the core's settled outcome: an operation that finished first returns its result, and the handle it acquired is owned.
- **Retryability and commit state.** Failures carry the core's own answer, never one derived from the code. `retryability` is `caller-decides` only for an aborted or timed-out operation that the core never dispatched, or that commits nothing (reads, discovery, connect, subscribe), and for a connect whose link the OS could not establish (CoreBluetooth `connectionTimeout`/`connectionFailed`, WinRT `Unreachable`, BlueZ `Failed`/`ConnectionAttemptFailed`; 5.0); a write that was dispatched is always `never`. A failure the OS answered carries the OS's own identity in `platform`, as on the Node desktop path: CoreBluetooth `{domain: 'corebluetooth', code}`, WinRT `{domain: 'winrt', code, metadata: {hresult, gattStatus}}`, BlueZ `{domain: 'bluez-dbus', code}`, with the OS message (or the core's detail) as `safeMessage`. Metadata values are strings, numbers or booleans; an integer JavaScript cannot hold exactly crosses as its decimal text. A failure without OS detail keeps the Tauri 4.x `{domain: 'btleplug', code: 'native-error'}` shape around the core's detail. Every failure also carries `commit`: `not-dispatched` (nothing reached the radio), `uncertain` (a dispatched write may have reached the peer — read the value back before deciding anything), or `null`.
- **Release.** Scan stop, unsubscribe, disconnect and window teardown keep the resource, and the native identity a retry needs, until the core confirms the release or answers that the resource is already gone. A failed release is reported and the next release calls native again; delivery pauses while a release is in flight. A resource the plugin admitted for a window that went away meanwhile (a cancelled or refused scan, connect or subscribe the core still carried out) is released by its own core identity. If that release fails, the plugin retries it automatically, as Tauri 4.x did: 8 attempts in all, the retries 100 ms apart and doubling up to 5 s. Until one lands the resource stays owed. The window's next release (and plugin shutdown) also retries it and reports a failure as `tauri.release.orphan`, or as `tauri.quarantine.exhausted` once all 8 automatic attempts were refused. A window released while retries are pending does not stop them, so an orphaned link or scan does not outlive a closed window just because its release failed once. Why a gone device object reports `released` (T-R1): a disconnect whose radio answer is that the device object is gone (BlueZ `org.freedesktop.DBus.Error.UnknownObject` / `org.bluez.Error.DoesNotExist`) is the operation's own answer that there is nothing left to release — the object never comes back, so every retry would fail identically and no later `Released` event can arrive. This differs from a disconnect whose deadline expired: there the platform said nothing, so the outcome stays `operation.timed-out` with retained `Disconnecting` ownership and the later OS release surfaces as a lifecycle `Released` event. A transport failure (`Timeout`, `NoReply`, `Failed`) likewise proves nothing and keeps the release pending.

## Lifecycle events and notification delivery

The core publishes typed connection-lifecycle events and the plugin forwards them to the connection-event stream the webview opened, matched by peer and connection generation (events for an older connection of the same peer match nothing):

- link loss the OS reports while the app is idle: `connected → lost`, cause `peer-link-loss`, then the stream ends `connection-lost`;
- a requested disconnect: `disconnecting → disconnected`, cause `requested-disconnect`, then the stream ends `owner-released`;
- a GATT service change: the database of that connection becomes stale (later GATT calls fail with `gatt.stale-handle`; rediscover);
- an adapter loss (see below): `connected → lost`, cause `adapter-loss`, then the stream ends `connection-lost`.

A loss that happens before the stream is ready is delivered right after the initial `connected` event. If the plugin falls more than 256 lifecycle events behind, every connection-event stream ends with `overflow` rather than skipping a transition.

A notification stream ends with the core's own reason: `connection-lost`, `service-changed`, `overflow`, or `source-failed` when the core closed a stream that is still mapped (with `operation.reset` when an adapter loss ended it). When the app itself released the link (`connection.disconnect`), the stream ends `owner-released` — the vocabulary's requested-disconnect word, as on every other host — so a connection supervisor backs off and reconnects instead of stopping. A scan the OS or an adapter loss ended without a stop request ends `source-failed` with the core's own words (`scan.start-failed`, platform detail), or `closed` when the OS stopped it without an error; nothing is left to stop. Each value carries the `delivery` the radio reported for the subscription (`notification`, `indication`, or `unknown` when the platform does not say); the subscribe response reports it too.

Delivery requirements are planned from characteristic properties before radio
effects. `prefer-notification` and `prefer-indication` impose no hard requirement.
A characteristic offering only notification or only indication can satisfy that
mode on every supported desktop platform; requiring its absent property fails
with `gatt.property-not-supported`. When both properties are present,
CoreBluetooth and BlueZ enable notification, so `require-notification` succeeds
and `require-indication` fails with `capability.limited` before subscribing.
WinRT can select either mode through the adapter's CCCD write; a failure to
enforce the selected mode is surfaced, not silently accepted. A platform with no
documented dual-property rule refuses a hard requirement with
`capability.limited`. The planner is `crates/ubm-desktop/src/delivery.rs`;
applications must not drop a delivery requirement just because they use Tauri.
Planning is separate from observation: notification values preserve the native
host's reported `delivery`, including `unknown` where the platform cannot report
the mode.

The maximum write length (`connection.maximum-write-length`, per `mode`) is the core's answer for that write mode, the same limit a write of that mode is admitted against: a reported maximum is never refused. Windows admits ordinary OS-managed with-response writes up to 512 bytes; commands use `GattSession.MaxPduSize` − 3. Linux admits with-response values up to 512 bytes and, with a reported MTU, commands up to MTU − 3. When BlueZ withholds the MTU, it admits both write modes up to 512 bytes and lets the OS answer the write; that is an admission limit, not an invented MTU measurement. macOS uses the OS-reported per-mode maximum. Tauri 4.x reported `mtu - 3` for every mode.

Ordinary `with-response` writes within the admitted maximum use the OS-managed
write procedure; Windows and Linux can therefore accept a value larger than one
ATT payload. This is distinct from caller-controlled prepared/reliable transactions:
the explicit `long-write` mode has no prepared-write radio path and is refused
with `capability.limited`, never silently converted to an ordinary write.
`no-prepared-write-path` is the capability limitation id, not the public error code.
`gatt:maximum-write-length` and `gatt:long-write` retain the instantiated desktop
core's capability reasons and limits; a limited descriptor does not promise every
transaction mode. The effective MTU (`connection:effective-mtu`) is the core's
measurement of the live link through the desktop central, like the desktop and
Electron hosts. If BlueZ omits the live MTU, the query answers
`capability.unavailable`; a backend without an effective-MTU mechanism answers
`capability.unsupported`. Neither case invents a measurement of 23.

Connected RSSI (`connection.rssi`) is the OS measurement of the live link, read through the core; a radio that cannot measure it answers `capability.unsupported`.

## Adapter

On Linux, the shared native authority resolves and pins the current unique
D-Bus owner of `org.bluez`; applications do not have to obtain or supply that
owner. The maintained daemon integration must still be installed explicitly:
the selected adapter must answer `LinuxAuthority1.GetContract` with the supported
lease/GATT revisions. Introspection alone does not prove implementation. See
[BlueZ deployment](BLUEZ_DEPLOYMENT.md) for the explicit privileged deployment
boundary; neither the plugin nor a renderer installs a daemon or falls back to
device-wide connection control.

`BtleplugDispatcherOptions::connection_policy` is an optional stricter owner
restriction. Set it to
`Some(tauri_plugin_unified_ble_manager::BluezConnectionPolicy::LeBearer { daemon_unique_owner })`
only when trusted Rust setup deliberately requires that exact D-Bus owner.
It is not an implementation attestation. A mismatched or replaced owner is
refused; omitting the option delegates owner binding to native authority.
See [BlueZ setup and migration](NODE.md#bluez-connection-policy-bus-and-pairing-generation).

The bootstrap capability snapshot preserves the instantiated central's
connection refusal and reason, including missing daemon authority; the webview
does not receive a generic platform-level connection claim in its place.

The attachment and `adapter.state` come from the one shared central; the plugin opens no second btleplug manager (on macOS, no second `CBCentralManager`). `BtleplugDispatcherOptions::adapter_id` names the adapter by the identity `ubm_desktop::btleplug_backend::list_adapters` reports (BlueZ `hci0`, the Windows adapter device id, `CoreBluetooth` on macOS). Without a name the sole adapter is used; with several adapters the first BLE operation fails `adapter.ambiguous`, and a name that matches none fails `adapter.selection-required`.

`adapter.state` reports what the OS reported through the core: `power` is `on`, `off`, `resetting`, `unsupported` or `unknown`; `unsupported` power makes the adapter `unsupported` with authorization `unavailable`; macOS `Unauthorized` is authorization `denied` with power `unknown`. `authorization` is `granted`, `denied`, `restricted` or `not-determined` where the OS has the concept (macOS, Windows) and `unknown` with the reason in `safeReason` where it does not (BlueZ). A removed adapter is `unavailable`. `heard` is the radio's own peer list.

Before any effect the core refuses radio work the adapter cannot do, per the legacy backend of that OS (macOS and Windows; BlueZ has no such gate and the OS answers): `adapter.powered-off`, `adapter.resetting`, `adapter.unavailable`, `permission.denied`, `permission.restricted`, `permission.not-determined`. These errors reach the webview exactly as the core made them: code, domain, operation, detail, retryability and commit.

When the adapter powers off, resets, becomes unsupported or unauthorized, is removed, or its daemon restarts, the core tears everything down: in-flight operations fail `operation.reset`, the scan and every notification stream end `source-failed`, every link ends with `adapter-loss`, and the attachment moves to a new backend and adapter generation (`tauri-backend-generation-{n}`, `tauri-adapter-generation-{n}`, numbered from the plugin's one counter as Tauri 4.x numbered them). The webview's manager survives the loss. The plugin, never the webview, rebinds every window bound to the replaced attachment and announces it on the reserved `attachment` stream: one item `{kind: 'value', value: {kind: 'backend-restarted', schemaVersion: 1, previousAttachmentId, attachmentId, attachment}}`. The webview adopts only an announcement for its own lease that names the attachment it holds, on the same backend instance; it never picks an attachment, and a route naming one the plugin did not give it fails `protocol.violation`. Until the announcement, and for the replaced attachment afterwards, every request fails `backend.reset` before any native call, except releases (`scan.stop`, `gatt.unsubscribe`, `gatt.database.release`, `connection.disconnect`, `connection.events.unsubscribe`, `operation.cancel`) and `manager.destroy()`, which still settle what the webview holds (a release of something the loss already ended answers `released`). Connection events keep reporting the attachment the link lived on. A connection supervisor waits for the adapter and reconnects through the same manager, however long the adapter stays off. No adapter-state event is pushed; read `adapter.state` for the current facts. Before 5.0 the webview had to recreate its manager after a loss.

Filter in the webview with `advertisementPassesViewFilter` (name or peer id, min/max RSSI, service UUID, manufacturer company id, named-only). Observations include `serviceUuids`, `manufacturerData`, `txPowerLevel`, and `serviceData` in addition to `peerId` / `localName` / `rssi`.

## Rust plugin (interim checkout setup)

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        )
        .build(),
    )
```

Grant `unified-ble-manager:default` only to intended windows. Await `manager.destroy()` when the webview session ends.

Security permissions are separate from the default transport permission. The
plugin defines `unified-ble-manager:allow-security-state`,
`allow-security-pair`, `allow-security-cancel-pairing`,
`allow-security-unpair`, and `allow-security-custom-ceremony`; each is enforced
by a Rust command scope, never by renderer request fields. The
default transport permission grants none of these security scopes. Grant only
the operations an intended window needs. Public `manager.security` operations
use the authenticated IPC routes and delegate to the instantiated native authority;
renderer payloads cannot grant these permissions. State/watch, pair, pairing
cancellation and unpair preserve that authority's result and error rather than
returning a transport-invented answer.

Availability is backend-specific. In particular, CoreBluetooth does not expose
explicit bond-store control; Linux and Windows expose only the security facts
and ceremonies their implemented native adapters can observe or perform.
Custom-ceremony permission does not by itself implement a ceremony on a native
adapter lacking it. Inspect `manager.capabilities` and preserve a native
`capability.unsupported` refusal. Routing tests and compile checks do not promote
hardware-evidence labels.

See [`example-tauri/`](../example-tauri/) for a small public-API proof.

## Native continuation owned by the Rust host

Keep a clone of the **same** `BtleplugDispatcher` passed to `PluginBuilder`.
Its trusted Rust methods run continuation on the dispatcher's existing central;
they do not construct another radio or grant a webview new IPC authority.

```rust
use tauri_plugin_unified_ble_manager::{BtleplugDispatcher, PluginBuilder};

let dispatcher = BtleplugDispatcher::default();
let continuation_host = dispatcher.clone();
let builder = tauri::Builder::default()
    .plugin(PluginBuilder::new(dispatcher).build());
// Retain continuation_host in your trusted application state. From an async
// startup or explicit OS-wake integration, call its methods below.
```

`continuation_host.continuation_execute(peer_id, declaration_json).await` accepts
the exact peer identity reported by the central; do not rewrite its casing. Use
the same `onAppearance: "native"`, known `peerId`, and canonical UUID/occurrence
`resubscribe` declaration as [Node continuation](NODE.md#native-continuation-in-a-trusted-process-host).
`continuation_describe_backlog().await` reports the queued-data count and the
shared native supervisor's last outcome. Native collection and permitted
recovery continue when the webview is absent, as long as the Rust host lives.

For handoff, call `continuation_prepare_claim(max_items, max_bytes).await`.
Validate and retain all returned batches and their loss accounting **before**
calling `continuation_acknowledge_claim(claim_token).await`. Repeated prepare
before acknowledgement returns the same prepared batch. An incomplete prefix
is acknowledged before preparing its retained tail; a refused disposal remains
owned and retryable. Never acknowledge data you could not decode, and never
equate a requested release with a successful receipt.

These methods return structured `Result<serde_json::Value, serde_json::Value>`
answers; preserve failures rather than replacing them with an empty backlog.
No renderer route is added: expose host-approved operations only through your
own authenticated application policy. On final application shutdown,
`authority_shutdown().await` stops native recovery and reports central cleanup.

The default queue is volatile, but a trusted Rust host can opt into durable
recording. Call `continuation_configure_recording_directory(&private_path)`
before a declaration containing `recording: { id, maxBytes, maxRecords }`.
The independent journal registry is available without radio initialization:
`continuation_recording_status(id)`,
`continuation_recording_prepare(id, max_items, max_bytes)`,
`continuation_recording_acknowledge(id, token)`,
`continuation_recording_stop(id)` and `continuation_recording_clear(id)` are
async trusted-host methods. Save/process a prepared prefix before explicitly
acknowledging it; a native claim never consumes durable records. Recording stop
does not release a radio. The same registry is used if a radio owner is later
created, rather than opening an unrelated store beside it. Paths are host-only,
and these methods do not create a new renderer route. The journal is plaintext,
bounded and explicit about storage failures; see [background recording](BACKGROUND.md).

Collection still requires the Rust process to run: durable disk records do not
make OS relaunch automatic. The application supplies any OS startup/wake registration and
persists its standing declaration; UBM does not install or escalate a service.
The bounded native queue reports overflow and cutoff loss. Physical-radio
qualification remains separate from deterministic and compile evidence.

## Maintainers

[Current 5.0 authority](README.md#current-50-authority), [`PLATFORMS.md`](PLATFORMS.md).

Parameter source recovery, retained failure delivery and bounded queue-gap
ordering follow the shared [connection observation ownership rules](NODE.md#connection-observation-ownership).
Windows PHY reads use the same [runtime-probed observation route](NODE.md#windows-phy-observation);
PHY requests and selection remain unsupported.

IPC version 6 requires nullable service graph facts and the native
readiness/acquired-GATT route contract. An older webview or plugin is refused
at negotiation before radio ownership is admitted.
