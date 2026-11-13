<!-- docs/WEB.md -->

# Web Bluetooth

Use `unified-ble-manager/web` to run the public UBM manager directly in a browser. The Web backend uses the browser’s device chooser instead of continuous scanning, then exposes the same connection and GATT handles as the other UBM hosts.

The complete runnable TypeScript/Vite application is in [`example-web/`](../example-web/).

This guide targets `5.2.3`; verify publication in the npm registry. Web Bluetooth support still depends on the browser, operating system, adapter, and peripheral. UBM reports those runtime boundaries; it does not fabricate a fallback backend.

## Requirements

- Use Chrome or another browser that implements Web Bluetooth.
- Serve the page over HTTPS. `http://localhost` is also treated as a secure development context.
- Start the system chooser from a click or another transient user activation.
- If the page is inside an iframe, its container must grant the `bluetooth` Permissions Policy.
- Keep the requested peripheral near the browser and ensure another central is not already occupying its connection.

Web Bluetooth does not provide UBM’s continuous `scan()` capability, background monitoring after the page closes, or process-level restoration. `manager.scan()` therefore rejects with `capability.unsupported`; use `manager.choose()` (often named `ble.choose()` in smaller examples).
The Web factory rejects `background` and `restoration` declarations before
creating a manager. It accepts the browser adapter identity, optional instance
identity, diagnostics limits, and caller-supplied entropy for ephemeral IDs.

## Install and create the manager

```sh
pnpm add unified-ble-manager@5.2.3
```

```ts
import { createWebBleManager } from 'unified-ble-manager/web'

const manager = await createWebBleManager()
```

The factory returns the same host-neutral `BleManager` interface used elsewhere. Importing `unified-ble-manager/web` is explicit: failure to load Web Bluetooth never substitutes React Native, Node, a simulator, or a mock.

## Ordered recording (in development)

The Web factory shares a browser-partition recording journal with
`createWebRecordingController()` from `unified-ble-manager/web`. Web Locks,
IndexedDB, and Web Crypto are needed to acquire and retain recording authority;
when the browser cannot establish authority, the start reports the observed
failure. A hidden page alone does not release an active connection. Browser
freezing, unloading, storage failure, and connection loss can still interrupt a
recording; these paths are not physical-radio qualification.

For a recording handoff, a retry of the same request can arrive while the
owner is still processing it. The owner retains a bounded replay request and
returns the same sealed response without executing the operation again.

If IndexedDB closes for a version change, an active recording can reopen the
compatible database before a new transaction. The active delivery path can
retry transient readiness failures while the recording still owns authority;
some final-cleanup and retirement retries remain under review. An
already-dispatched transaction retains its actual outcome, and an ambiguous
commit is reconciled before acknowledgment. An incompatible schema reports a
failure rather than being rewritten. These behaviors have deterministic test
coverage. An active recording with a synthetic Bluetooth device has also been
tested in Chrome with IndexedDB upgrade events and Web Locks; that test does
not establish physical Bluetooth behavior or qualification in other browsers.

The Web journal opens IndexedDB schema version 5. Compatible version 2, 3, and 4 schemas
upgrade without rewriting their records; version 5 appends separately-owned terminal
evidence stores while retaining every existing journal, marker, and reservation row.
The version increase prevents an older version 2, 3, or 4 database opener from
subsequently opening the upgraded database. An existing connection must close before
the upgrade can complete. The reader can
replay ordered-recording protocol v1 and v2 rows, ending a prepared batch at a
version boundary. New Web recordings select v2 and retain that selection in the
recording context, start and stop receipts, and record metadata. Historical v1
rows remain readable and writable. Ordered platform metadata entries in Web
start failures and v2 journal records retain their order and repeated keys.
Web's ingress still reports its existing terminal and loss observations; it
does not yet produce the full series of individual failure observations described
in the ordered failure observations integration plan. Native producers and
transports require their own version negotiation before they can emit v2.

A sealed recording can be replayed and acknowledged with the controller, then
cleared once durable release and cleanup evidence is present. A live recording
can reserve marker writes. Its owner journals the reservation, source admission
and dispatch intent before invoking the browser write. Explicit cancellation
before dispatch prevents the write; after dispatch the observed browser outcome
is retained. Lost storage completions are reconciled using the exact operation
ID and the matching operation signature, while unknown outcomes remain debt.
A marker caller's deadline or cancellation bounds its wait; it does not prove a
pending storage operation failed. The live owner retains that operation and
continues cleanup when its outcome becomes known. A failure before an IndexedDB
transaction is created is definitive. Retained marker evidence counts against
the recording byte limit, and a reservation also holds space for its marker and
the Web source terminal. When that budget is exhausted, further reservations are
refused. Reader acknowledgement may still be needed to free record slots or
earlier backlog before the marker and terminal can be published.

These guarantees are scoped to the surviving live owner. The package cannot
transfer or abandon unresolved marker debt after that realm is lost or prove
exactly-once physical device execution. A released Web Lock, missing owner response
or elapsed time does not establish that the former owner or physical source has
stopped. `createWebRecordingRecoveryInspector()` reads the retained marker rows
for an exact recording incarnation and reconciles individual ledger operation
receipts. A found operation is reported as `observed` with `signature: 'unverified'`: the inspector lacks the live owner's independent expected transition, so it cannot establish that the signature matches. It reports legacy inventory and unsettled outcomes as unknown, and
reports source cleanup as unknown and former-owner activity as possible. The
single `inspect` call returns a capacity error above 256 marker rows.
For a known recording ID, `listIncarnations` returns its active incarnation and
pages through retained, cleared incarnations, even when the application did not
save the incarnation. `listMarkers` pages up to 32 marker rows and
`listMarkerOperations` pages up to 8 operation receipts, including operation ID,
transition data, owner generation and epoch, and the exact retained result and
request bytes. Private owner tokens are not exposed; the transition signature is
**unverified** and is not an authorization for a radio action. Use each page's
`next` as the next call's `after`. Each page is a separate transaction: concurrent
changes can make combined pages incomplete or inconsistent, so they do not form
a revision-fenced snapshot and cannot authorize an ownership decision.

A clear with marker evidence is admitted only while its storage partition has
fewer than 64 cleared marker anchors and the sum of their retained ledger
charges, including the new anchor, is at most 1 GiB. This is an accounting
limit, not a measurement of browser disk usage; active recordings retain their
own limits. Existing anchors are counted directly, including those written before
this policy. If old evidence already exceeds a limit, no evidence is removed:
clear reports `stream.quota` until enough evidence has been exported and
explicitly disposed. To discover identities across recording IDs, call
`listIncarnations(anyValidId, { after: null, limit: 1,
includePartitionRetained: true, partitionRetainedAfter: null })`. Use the returned
`partitionRetainedNext` as `partitionRetainedAfter` for the next page. Each entry
contains its recording ID, incarnation and ledger charge. Export both marker and
operation pages for that identity before disposal; pages are independent
observations, and partial disposal makes the export incomplete.
Malformed anchor values are returned, untouched, with their IndexedDB key and
original structured-clone value in `malformedRetained` or
`malformedPartitionRetained`. A malformed legacy sequence is similarly exposed as
`malformedLegacyClearSequence` when clear receipts are requested. Valid entries
remain available on the same page. Pagination counts stored rows, including
malformed ones, and the look-ahead row is inspected on the next page. Preserve
and export these values before any independent repair; they are not valid
receipts or proof that a clear occurred. A malformed anchor still prevents a
clear that needs to establish the existing retention charge.

Clear requires Web Locks and takes one non-queued exclusive lock per IndexedDB
journal before reading or allocating anything. The lock name uses the database
name, which is shared by separate connections in the same browser storage
partition. A custom database adapter without a `name` must supply the same
stable `journalIdentity` to every `createWebRecordingPartition` using that
journal; different journals need distinct identities. An unnamed adapter without
an explicit identity uses the default journal name. An explicit identity must
match the name when the adapter exposes one. Concurrent cooperating
clients report unavailable and can retry. A denied or unsupported Web Locks
implementation reports its platform reason. If acquired authority is lost,
the former owner fences later transaction dispatch. A transaction already
dispatched may still commit; loss does not prove it stopped. The clear caller
receives an unavailable result and must inspect durable outcomes.
Once dispatched work has settled, clear also waits for lock release to settle;
an unsuccessful or unconfirmed release is reported as unavailable even if the
deletion committed. Acknowledging a clear receipt atomically moves its exact
proof to retained storage, so an independent inspector cannot erase the proof
needed by a caller still reconciling.

A timed-out allocation is abandoned before deletion can be dispatched. A
completion that never arrives keeps the live owner until its outstanding work
settles; terminating that realm releases its Web Lock but does not establish
whether a transaction committed. A timed-out lock request abandons a late grant.
Every concurrently running client must use this coordination protocol; older
clients can bypass the lock and must not be run concurrently for this guarantee.

Clear uses one durable partition-wide operation sequence and fences the recording
incarnation. Its deletion and exact outcome receipt commit atomically. The
receipt remains even when clear returns successfully or its reply is lost, until
the application explicitly acknowledges it. Inspect receipts with
`listIncarnations(id, { after: null, limit, includeClearReceipts: true,
clearReceiptsAfter })`, starting with a null cursor and following
`clearReceiptsNext` until null. Each page includes up to `limit` new-format
receipts across the partition. Legacy slots are also inspected for compatibility;
they may be repeated on each page. Malformed receipts are reported separately
and remain untouched. `listClearProofs({ after: null, limit })` instead pages the
partition-wide key space, including legacy active receipts for every recording,
archived receipts, identified attempt outcomes and malformed rows. Follow `next`
until null, even when a page contains no entries; each page is an independent
observation. This lets a legacy `clear(id)` caller find an archived receipt by
its target recording ID and incarnation after losing its reply. Active and retired receipt storage is subject to browser
quota, with no configurable admission budget or fixed receipt count ceiling.

For an identifiable clear, call `prepareClearAttemptV1(id)` on the Web recording
controller and persist the returned recording ID, producer incarnation and
random attempt ID before calling `commitClearAttemptV1(attempt, { signal })`.
`clearAttemptV1` is an alias for the commit method. Preparation reads the current
incarnation and does not delete the recording. Commit checks the exact identity
again under the clear lock; a different incarnation cannot be cleared by an old
attempt. Cancellation requests that work stop: before dispatch it prevents the
operation, while a transaction already dispatched may still commit. If the
result or lock release is uncertain, use the recovery inspector's
`getClearAttemptV1(attempt)`. `committed` returns the exact durable outcome;
`unknown` does not establish that an unsettled operation failed. Recovery reads
time out after five seconds and attempt to abort their IndexedDB transaction;
an unsettled underlying read retains its admission slot. Within one JavaScript
realm, named connections to the same journal share two read slots; unnamed
adapters are bounded per database object. This is not a cross-realm limit.
An optional abort signal can end the caller's wait without proving an outcome.
Retrying the
same committed identity returns the retained result. `acknowledgeClearAttemptV1`
verifies the complete receipt but retains it, so another inspector cannot erase
this attempt's outcome while a caller reconciles. These retained attempt receipts
are subject to browser storage quota and are not automatically retired.
If a retry reaches a serialized admission transaction after its recording
incarnation has disappeared or been replaced, the transaction installs a
terminal `superseded` fence for that exact attempt. The commit reports
unavailable and recovery returns `superseded`; this does not claim the attempt
cleared the recording. A delayed deletion must check the same proof slot and
cannot dispatch through that fence. Until this fencing transaction commits, an
absent receipt still means `unknown`.

For an opt-in V2 clear, retain the current recording ID and producer
incarnation (for example from `prepareClearAttemptV1`), and create and persist
one cryptographically random 32-byte request ID as 64 lowercase hexadecimal
characters. Call `admitClearAttemptV2({ recordingId, producerIncarnation,
requestId })` and persist the returned attempt before calling
`commitClearAttemptV2(attempt, { signal })`. Retrying admission with the same
three fields returns the same attempt, including after a lost admission reply;
reusing the request ID for different fields fails. The commit checks the
incarnation and ownership evidence under the Web clear lock, and writes the
complete successful result to the proof ledger in the same IndexedDB
transaction as the deletion. `inspectClearAttemptV2` reports `pending`,
`committed`, `cancelled`, or `retired-or-unknown`. An uncertain call is not
proof of failure: inspect or retry the exact attempt. A committed retry returns
the recorded result. `cancelClearAttemptV2` serializes with deletion; a
cancelled attempt cannot later delete the recording.

Ledger metadata records the number of issued V2 request fences, and an absent
request is admitted only when the stored fence count agrees. Missing metadata
with a surviving ledger marker, proof, or fence fails closed. For the new
format, a missing marker or fence also fails closed. Surviving rows remain
untouched. Metadata written by the earlier ledger format did not record that
count: exact existing request retries remain available, but a new request on an
established older ledger fails closed because a missing fence cannot be ruled
out. A ledger whose old metadata disappeared before the marker was installed
and which has no surviving staged proof or fence cannot be distinguished from
an empty ledger. Corruption that changes multiple rows consistently is also not
detected by the count.

V2 admission requires Web Locks, including shared locks and `query()`, and
IndexedDB. Participating tabs and workers use a shared set of admission slots
per journal; the default limit is two. Custom partitions can set
`clearAdmissionLimit` to a positive safe integer in `createWebRecordingPartition`.
All active participants for a journal must use the same limit. A conflicting
limit is refused; retry after existing admissions settle to change it. A busy
coordination lock or a full slot set also returns unavailable and can be retried.
This policy is separate from the durable proof and request-fence limits below.
Older implementations do not participate in the admission bound.

A caller receives an uncertain result after five seconds, independently of the
transaction. A slot and its policy lock remain held until the transaction
actually completes, aborts, or fails to start. Before reusing a slot, its next
owner completes an overlapping IndexedDB readwrite transaction in
`readerReceipts`. This fences earlier transactions even if their realm has
ended and the browser has released its locks. If that fence cannot complete,
the slot stays unavailable; elapsed time is not proof of settlement. A caller
whose wait expires before an admission transaction is dispatched does not start
one later, even if storage readiness resolves after the deadline. Loss of its
slot or policy lock also prevents a pending admission from being dispatched.
Retry the exact request ID, since a timeout after dispatch does not establish
failure and the durable ledger retains the admission identity.

If queueing a transaction or processing an IndexedDB callback fails and abort
also fails, the public error retains a library-created primary failure's
classification and operation. Browser quota and version errors retain their
recognized classifications. IndexedDB platform metadata contains an ordered `errors` array for
the primary and abort failures, including available nested error structure.
Only recognized error names are published; arbitrary names, messages and thrown
values are redacted. Typed errors are trusted only when created by the Web
recording implementation; an adapter-thrown value cannot supply the public
operation, message or platform detail by imitating a typed error. The same
boundary applies to recovery inspection, recording setup and lock establishment.
Combined failures retain their primary classification and bounded evidence only
when the recording implementation created the combination. A single bound applies to the complete evidence graph;
omitted or cyclic details are marked. The encoded result stays below the 1 MiB
platform failure envelope limit. The admission slot remains owned until the
transaction actually settles, even if the caller has already received an error.

A readiness failure keeps its underlying safe classification in a start receipt;
only a library-origin incompatible-schema failure suppresses autonomous readiness
retry. A commit proven absent after reconciliation retains a quota classification
and leaves the live recording waiting for capacity; a transient readiness failure
at the same boundary retains its autonomous retry. If a reconciliation read
fails or completes late, the exact delivery and its
original failure remain owned until the outcome is known. When a later read
proves a transient readiness failure did not commit, the live owner retains the
failure in its runtime diagnostic and resumes that same delivery without another
backoff delay. A later delivery's new failure follows the normal backoff.
`createWebRecordingController().runtimeDiagnostic(id)` returns the live owner's
current supervisor state and its most recent redacted failure classification,
including setup and enable failures and source, member and authority cleanup
failures. It remains available after a successful retry while the owner is live.
Cleanup errors retain a trusted specific classification when available; otherwise
they use a safe cleanup classification. A failed registration's cleanup is
classified from the cleanup failure, not the original installation failure;
the setup receipt follows the installation failure even when registration errors
are nested. Repeated rollback attempts classify the latest attempt while retaining
earlier errors internally as evidence. Reservation, installation and enable failures use the setup phase;
authority preparation, journal opening and terminal observation retain their
own failure classification. Membership removal during rollback or installed-source detach
can be retried after an ambiguous failure: cleanup retains the exact outstanding
registrations, and a repeated removal leaves other registrations on the characteristic intact.
Until removal succeeds, a fenced V2 group remains an observation owner: its
physical dispatch route stays present during and after an unsuccessful removal,
so callbacks are retained as failure observations before its terminal, even if
another database owns the remaining physical subscription. For V1 and ordinary
groups, a removal failure does not suppress the terminal or other groups'
terminals; later retirement retries the outstanding registrations. Concurrent
sink and removal failures, including repeated removal failures during overflow,
are reported together.
An authoritative late storage success resumes independent retained deliveries
even if member cleanup has separately failed. That cleanup remains owned and
requires its own successful retry before terminal publication and authority
release. An unstarted source release failure is likewise cleanup debt; only a
failure of journal finalization is treated as a storage failure. A late successful
terminal observation resumes authority release without a further controller call,
provided authority and acknowledgement remain valid. The production observation
attempt is bounded to five seconds. The original transaction remains owned after
the timeout, including while database readiness is pending. An exact-identity
reconciliation read waits until that particular write transaction has actually
been created; a read issued before dispatch cannot prove its absence. The later
overlapping read may reconcile it without issuing a second observation write. It
distinguishes the exact committed receipt, proven absence, and a conflicting or
otherwise unknown receipt. Proven
absence permits a retry even when the abort callback was lost; a conflict or failed
read never authorizes another write. Prompt and late read failures remain in the
redacted uncertainty evidence alongside the original failure, including after
authority loss or cleanup failures. Its completion resolves the storage outcome even if authority has since been lost;
a confirmed transaction abort or failure before transaction creation proves that
attempt did not commit. An abort request alone or an ambiguous rejection does not
prove non-commit. A proven observation failure is reported as an IndexedDB storage
failure; unknown outcomes remain `reconciliation-unknown`. The stop result retains
any earlier setup failure alongside unknown storage evidence. If authority is lost
before settlement, a late failure and the authority loss remain visible together;
a late success resolves storage uncertainty but cannot acknowledge the observation
or release stale authority. An unstarted recording
can retry its terminal observation after a late rejection. The observation
does not decide whether a separate blocked storage operation is retried. This bounded observation is held
only in memory, is marked `persisted: false`, and is unavailable once the live owner is retired or
in another browser realm. It does not provide cumulative loss or record counters;
`status()` remains unavailable when those cannot be established truthfully.
If a timed-out read
later rejects, bounded, redacted evidence of that rejection is retained alongside
the timeout. While the outcome remains unknown, `stopOrdered()` reports a
`storage` failure with kind `reconciliation-unknown`; its `detail` contains a
bounded JSON projection of recognized error names and aggregate structure,
including the original failure, timeout and late rejection when observed. Host
messages and arbitrary thrown values are excluded. If setup also failed, this detail
includes its redacted `setupFailure` classification alongside the outstanding storage
evidence; the original start receipt retains that setup failure. A proven absence
restores that failure's classification; transient readiness can resume recovery,
while quota waits for capacity. A still-unknown outcome does not authorize a
second unguarded write. During notification enable, library-observed link loss,
owner release, adapter loss and backend destruction retain their own event
classification in the setup receipt. Browser-thrown errors remain normalized as
Web Bluetooth failures.
If recording enable waits for
a notification stop that fails, its setup receipt reports the library's GATT
cleanup failure without trusting typed errors supplied by the browser. A stop
fences a recording waiting for a previous physical stop, so that recording cannot
dispatch a later notification enable. Each consumer's cancellation and deadline
are checked at physical notification dispatch; another live consumer may still
authorize a shared start. Once a recording start has been dispatched, its
supervisor owns the source: cancellation or expiry of a start waiter does not
itself stop that recording, while a recording stop fences its source. Web lifecycle error logs project
recognized names and bounded nested cause/aggregate structure instead of browser
messages or arbitrary thrown values; these logs are diagnostic evidence, not a
replacement for the operation's result or cleanup receipt.

### Durable terminal evidence recovery

When terminal evidence is requested, Web first persists the exact canonical
terminal bytes and digest as `staged`, then atomically promotes that row to
`committed`. `retryWebTerminalEvidence(partition)` rehydrates staged rows from
the selected partition after a backend or application restart and retries that
promotion. It rejects the storage failure; a successful retry never means that
an unresolved row was ignored. `createWebTerminalEvidenceRecovery(partition)`
provides partition-scoped `list`, bounded `read`, explicit `acknowledge`, and
`retry` operations. A reserved row has no terminal bytes and remains visible
until its owner explicitly retires it; recovery never fabricates a terminal or
silently frees its quota. Recovery acknowledgement accepts only a `committed`
row: it rejects reserved and staged rows, which remain owned by reservation
release and durable retry respectively.

`listWithStatus()` returns currently Web-Lock-available retained rows under
`records`, plus a sorted `busyProducers` list for any producer with a row held
by another live tab or runtime owner. It does not expose the locked row's
scope, reference, or owner. `list()` remains the complete inspection view and
does not acquire locks. A malformed persisted terminal row rejects recovery;
it is never filtered into an apparently empty status result. The same
validation applies to list, status, read, retry, and acknowledgement paths:
the row identity, version, producer, scope, state, byte sequence, digest, and
reservation bound must describe one supported terminal-evidence row.

Retain an independent durable copy of the complete `committed` or `cancelled`
outcome before passing it to `retireClearAttemptV2({ outcome,
acknowledgeDurableCopy: true })`. Retirement requires the same clear lock as
commit, so it cannot pass an outstanding clear owner. It releases the ledger
outcome slot but keeps a durable request-identity fence: admission still returns
the original attempt and a retired attempt cannot dispatch a new deletion.
`retired-or-unknown` alone does not prove a particular outcome. Neither
cancellation nor retirement is automatic. If retirement's transaction does
not report completion within five seconds, the call reports an uncertain
result and retains its lock until that transaction settles; inspect the attempt
before retrying. A lost completion can therefore leave the lock unavailable.

`clearProofUsageV2` reports the retained V2 outcome count and permanent request
fence count, as well as `clearNamespaceRows`: the number of rows with keys
beginning `:ubm-clear-`, including V1 evidence, clear receipts and sequence
metadata. This is a row count, not a byte count or an inventory of all IndexedDB
storage. The fence count is compared with the independently stored count when
available; `legacy-unverified` means the older ledger has no such count.

`configureClearProofV2` uses the returned revision to set the outcome `limit`
and, optionally, a separate `fenceLimit`. Both default to unlimited; omitting
`fenceLimit` preserves its previous setting. Reaching the fence limit refuses
new V2 identities even if their outcomes were retired, but exact admission
retries still return the original identity. Increasing or clearing the limit
requires a revision-checked configuration update. Fences and V1 evidence are
never automatically deleted. This policy is not a partition-wide byte quota:
the clear namespace and other stores have no limit here. Older writers do not
enforce this policy; older implementations that reject the extended metadata
cannot use the V2 ledger after a fence limit is set. Browser quota still
applies, and an IndexedDB abort commits neither deletion nor its V2 result.

To acknowledge an exact receipt, pass its `targetRecordingId`,
`producerIncarnation` and receipt to `disposeRetained(targetRecordingId,
incarnation, { acknowledgeExport: true, limit: 1, clearReceipt })`. This compares
the complete receipt and atomically preserves a copy before retiring the listed
receipt; it does not delete marker evidence. `more` with one deletion confirms
retirement; `absent` alone does not establish which caller retired it. To recover
after a lost acknowledgement, lock loss, or reload, pass the same receipt and
`verifyRetiredClearReceipt: true` to that method. This bounded read does not
change storage: `more` with `deleted: 0` includes the exact `retiredReceipt`,
while `absent` does not prove the clear failed or the active receipt is absent. A conflict or
malformed retained proof fails closed. Retired copies are available through
`listClearProofs` as well as exact verification. Neither
retired copies nor caller-known attempt receipts are automatically deleted, so
abandoned owners require no timeout-based guess about whether they are still
reconciling. Partition-wide retained-evidence quotas and interoperability with
older concurrent clients remain follow-up work. An older library can inspect
only legacy receipt formats, so use this version to discover new outcomes.

V1 and earlier clear evidence is not migrated or retired by the V2 ledger.
Removing a V1 attempt outcome while still allowing a missing V1 identity to
execute or become superseded can erase the only evidence of an earlier clear.
Retirement relies on the caller actually retaining the exact outcome durably;
IndexedDB cannot verify that assertion.

Inspection also reports `legacyClearSequence` when an older per-ID sequence row
exists. It remains a fence for older contexts that may still hold transactions;
the new partition-wide sequence does not create more of these rows. There is no
safe automatic retirement of that legacy fence while such contexts may exist.

After a normal, proven clear, inspection reports
`cleared-marker-evidence-retained` and the original ledger byte charge. The
application can export the marker and operation pages, then explicitly call
`disposeRetained(id, incarnation, { acknowledgeExport: true, limit })` repeatedly
until it reports `disposed`. Each transaction deletes at most `limit` rows (1–32),
operations first, then markers, and finally the retained anchor. A transaction
failure leaves that batch uncommitted; after an uncertain completion a repeat may
report `absent`, which does not prove which caller removed the anchor. Partial
disposal makes inspection incomplete. The application is responsible for preserving
its export before acknowledging it. Disposal refuses active incarnations and only
accepts a retained anchor created by proven clear; it does not resolve or remove
owner-loss evidence or authorize takeover.
It does not provide a full-state revision, abandon a recording, archive data,
or delete an archive. Durable marker recovery and explicit uncertain
abandonment are not yet implemented.
Cold takeover of an abandoned live producer and recording reclaim are not yet
implemented, and complete live aggregate status is unavailable. The Web
recording implementation remains in development and has not been qualified
across browsers or physical devices.

## Complete browser lifecycle

This example chooses a standard Heart Rate Service peripheral, grants optional access to Battery Service, connects, discovers GATT, reads Battery Level, and consumes Heart Rate notifications.

```ts
import { BleError, type BleManager } from 'unified-ble-manager'
import { createWebBleManager } from 'unified-ble-manager/web'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'

let manager: BleManager | null = null

async function chooseReadAndSubscribe(): Promise<void> {
  manager = await createWebBleManager()

  try {
    // Keep this call directly in the click-handler path.
    const peer = await manager.choose({
      filters: [{ serviceUuids: [HEART_RATE_SERVICE] }],
      optionalServices: [BATTERY_SERVICE],
      timeoutMs: 60_000
    })

    const connection = await manager.connect(peer, { timeoutMs: 60_000 })

    try {
      const gatt = await connection.discover({ timeoutMs: 20_000 })

      const battery = gatt.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
      const batteryPercent = parseBatteryLevel(await battery.read({ timeoutMs: 10_000 }))
      console.log('Battery', batteryPercent)

      const heartRate = gatt.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC)
      const subscription = await heartRate.subscribe({
        timeoutMs: 10_000,
        delivery: 'prefer-notification',
        stream: 'balanced'
      })

      try {
        for await (const event of subscription.values) {
          if (event.kind === 'value') {
            const measurement = parseHeartRateMeasurement(event.value.value)
            console.log('Heart rate', measurement.beatsPerMinute)
          } else if (event.kind === 'overflow') {
            console.warn('Dropped notification items', event.droppedItems)
          } else {
            console.log('Subscription ended', event.reason)
            break
          }
        }
      } finally {
        await subscription.remove()
      }
    } finally {
      await connection.disconnect()
    }
  } catch (error) {
    if (error instanceof BleError) {
      console.error({
        code: error.code,
        domain: error.domain,
        operation: error.operation,
        browserCause: error.platform?.metadata.browserErrorName,
        retryability: error.retryability,
        commit: error.commit,
        recovery: error.recovery
      })
    }
    throw error
  } finally {
    await manager.destroy()
    manager = null
  }
}

document.querySelector('#connect')?.addEventListener('click', () => {
  void chooseReadAndSubscribe()
})
```

For an application with separate Connect, Disconnect, Reconnect, and Destroy controls, use the runnable [`example-web/src/main.ts`](../example-web/src/main.ts).

## Filters and service permission

Chooser `filters` decide which devices the browser may display. Separate filter objects are OR branches; the fields inside one filter are AND constraints.

```ts
const peer = await manager.choose({
  filters: [{ serviceUuids: [HEART_RATE_SERVICE], namePrefix: 'My device' }],
  optionalServices: [BATTERY_SERVICE],
  timeoutMs: 60_000
})
```

`optionalServices` grants later GATT access to services that were not part of the matching filter. It accepts service UUIDs, not characteristic UUIDs. Request every service the application will access, but do not request unrelated services.

The chooser itself belongs to the browser. UBM cannot auto-select a new device, extend the browser’s native discovery UI, or tell whether Chrome’s `NotFoundError` came from an explicit Cancel action versus the chooser closing without a compatible selection. UBM preserves the browser cause in `error.platform` and reports the stable public code `chooser.cancelled`.

## Peer names and descriptor-less characteristics

Web Bluetooth exposes no advertisement payload for chooser-selected devices, so the chosen peer's `name` is the browser's `BluetoothDevice.name` verbatim — and `null` when the browser withholds it (Chrome reports it; other browsers may not). A `null` name never means "no device": the peer is still the authorized chooser selection.

Discovery treats a characteristic with no descriptors as an empty descriptor list. Web Bluetooth's `getDescriptors()` rejects with `NotFoundError` in that case (seen on Chrome with the Polar H10); UBM reports the characteristic with zero descriptors instead of failing discovery. Any other descriptor error still fails discovery with its own code.

## Previously authorized devices

When the browser exposes `navigator.bluetooth.getDevices()`, UBM projects those origin grants through the peer directory:

```ts
const manager = await createWebBleManager()
const authorized = await manager.peers.authorized({ timeoutMs: 10_000 })

if (authorized.length === 1) {
  const connection = await manager.connect(authorized[0], { timeoutMs: 60_000 })
  await connection.disconnect()
}
```

An authorized Web peer means the current origin has permission to address that browser device. It is not an Android-style system bond, proof that the device is nearby, or proof that another central is not connected. Do not guess when several authorized peers exist; ask the user to choose explicitly or use an application-owned persisted `PeerReference` and `manager.peers.resolve()`.

Origin authorization can survive a reload, but an in-memory peer or connection handle cannot. Create a new manager after reload and query or resolve the peer again.

## Deadlines, cancellation, and safe retry

All public operations accept `timeoutMs`; cancellable operations also accept `signal`. Each timeout applies to that operation, not to an entire multi-step journey.

```ts
const abort = new AbortController()
const connection = await manager.connect(peer, {
  signal: abort.signal,
  timeoutMs: 60_000
})
```

There is one important browser boundary: `BluetoothRemoteGATTServer.connect()` returns a native promise with no cancellation API. If `manager.connect()` reaches its deadline, UBM reports `connection.failed` (`caller-decides`, platform `web-bluetooth`/`DeadlineExpired`, bound in `deadlineMs`) and releases the pending attempt; a late native success is disconnected by compensation, so a retry on the same manager and peer is a fresh attempt, never `connection.already-owned`.

The pending attempt is released when the failure settles, so a retry cannot overlap it; a late native success is disconnected by compensation:

```ts
try {
  await manager.connect(peer, { timeoutMs: 60_000 })
} catch (error) {
  if (error instanceof BleError && error.code === 'connection.failed' && error.operation === 'web-connection.connect') {
    // The pending attempt was already released; retry on the same manager and peer.
    return manager.connect(peer, { timeoutMs: 60_000 })
  }
  throw error
}
```

Clearing ownership early would permit overlapping browser connections and is intentionally not supported.

## Notifications and cleanup

Subscription values are a bounded async stream. Handle all three event kinds:

- `value`: a notification or indication payload;
- `overflow`: visible data loss because the consumer did not drain fast enough;
- `terminal`: the stream ended because of removal, disconnect, adapter loss, or teardown.

Release resources from the inside out:

1. `await subscription.remove()`;
2. `await connection.disconnect()`;
3. `await manager.destroy()`.

Each cleanup returns a receipt. Production applications should verify `state === 'released'` and preserve any reported cleanup failures. Also destroy the manager during page teardown as a best-effort safeguard, while keeping primary cleanup in explicit application actions.

## Errors users can act on

Catch `BleError` from the root package and retain its structured fields. Do not replace every failure with one generic message.

| Field          | Meaning                                                                                        |
| -------------- | ---------------------------------------------------------------------------------------------- |
| `code`         | Stable UBM category such as `chooser.cancelled`, `operation.timed-out`, or `connection.failed` |
| `domain`       | UBM subsystem such as `chooser`, `connection`, or `gatt`                                       |
| `operation`    | Exact operation boundary, for example `web-connection.connect`                                 |
| `platform`     | Safe browser detail, including `browserErrorName` when Chrome supplies one                     |
| `retryability` | `never` or `caller-decides`: the operation's own answer about repeating it                     |
| `commit`       | `uncertain`, `not-dispatched`, or `null`: whether the failed operation may have taken effect   |
| `recovery`     | Recovery disposition and suggested actions, following `code`, `retryability` and `commit`      |

Common boundaries:

- `chooser.cancelled` + `NotFoundError`: the chooser ended without returning a compatible device;
- `connection.failed` + `NetworkError`: Chrome selected the device but could not open GATT; the link was not established, so it is `caller-decides` (5.0) and the backend never retries it;
- `connection.failed` + `DeadlineExpired` at `web-connection.connect`: the bounded native connection did not settle (`caller-decides`; every other operation keeps `operation.timed-out`);
- `gatt.not-found`: the connection opened, but a requested service or characteristic was unavailable or not granted;
- `connection.lost`: the browser reported the link gone (or failed a GATT call with `NetworkError`) during another operation;
- `operation.disconnected`: the app's own release cut another operation off;
- `operation.reset`: Bluetooth became unavailable during another operation (the link ends `adapter-loss`, its streams `source-failed`);
- `peer.not-found`: `connect()` named a peer the chooser never returned.

These are the same names every host uses for the same event (5.0; see "One name per physical event" in [`UNIFIED_SEMANTICS.md`](UNIFIED_SEMANTICS.md)).

## Iframes and deployment

A top-level HTTPS page needs no extra header. An embedded application needs its container to allow Bluetooth, for example:

```html
<iframe src="https://example.test/app" allow="bluetooth"></iframe>
```

The embedding server may also restrict or grant access using the `Permissions-Policy` response header. Browser and administrator policies can still disable Web Bluetooth.

## Testing

Application tests should inject `createWebBleManagerWithEnvironment()` only at a test boundary. Production code should use `createWebBleManager()`.

The repository example builds against the public package entrypoints:

```sh
pnpm build:example:web
pnpm example:web
```

Open <http://localhost:5173>. A successful local run is useful evidence, but it is not by itself a cross-browser or cross-platform support claim.

## Maintainers

See [`PLATFORMS.md`](PLATFORMS.md), [`PEERS.md`](PEERS.md), and [Current 5.0 authority](README.md#current-50-authority).
