# rc.21 end-to-end source reread

This is the chronological source review record. The completed runtime batch is
`9916500f`; final gate corrections add only the portable test logging dependency
and remove an iterator conversion in Windows directory collection. Both preserve
runtime behavior. Current execution evidence and all 35 acceptance records are
in [RC21_REMEDIATION.md](RC21_REMEDIATION.md) and its indexed closure ledger.
Earlier freeze and pending statements below apply to their named historical heads.

## Paths reread

- R2-02–06: BlueZ admission/commit/rollback/abandon and pre-lease operation
  lifetime, physical generation loss, deferred reconciliation; Rust lease
  reservation/recovery/release validation, protected release transfer and
  independently retained ACK maintenance. Accepted admission participates in
  protection before commit; loss cleans early interests before the no-peer
  return. Live held Pair success subsequently passes; see the final evidence ledger.
- R2-07–09 and R2-35: actual RN internal methods and provider dispatch, mobile
  wire write-when-ready, shared Rust FIFO admission and readiness wait, public
  IPC GATT helper forwarding. Input ownership precedes awaiting, and waiting
  uses the same admission/budget as ordinary writes. RN source failure closes
  readiness watches independently of native cleanup debt.
- R2-10–11: public IPC terminal rehydration, event identity and renderer clock;
  Tauri live admission and unsubscribe replay. Admission counts live watches;
  replay identity is separate from concurrency.
- R2-12–15, R2-31 and R2-33: shared per-fact scan evidence, complete identity, receipt-
  driven global expiry and peer/fact bounds; conservative WinRT ingress and
  concrete scan options; planning includes connectability. Downstream merged
  observations are not recached as fresh native evidence.
- R2-16–18 and R2-24–25: public/provider initial/live ordering and shared numeric
  validation; WinRT runtime method/event presence, native all-zero disconnected
  mapper, callback error preservation, vendor queue gap relay and source epochs.
  Buffered events supersede an unordered delayed initial sample; source gaps
  trigger explicit downstream reconciliation rather than replay stale samples.
- R2-19–21: distinct required/preferred delivery forwarding, native first CCCD
  selection and structured primary/handler-removal failures; Tauri service-level
  restriction comes from the service occurrence rather than a characteristic.
- R2-22: Apple discovery waits for inclusion callbacks, expands discovered
  included instances, and serializes actual primary/inclusion identity; native
  shared and Tauri projections retain nullable unknown graph facts.
- R2-23 and R2-26: native runtime preferred-preset admission/request lifetime;
  Windows known-plus-connected inventory, typed identities, transient object
  cleanup and bounded deduplication; Android system GATT retrieval; Apple
  service-scoped retrieval without delegate/cache/lease admission.
- R2-27: public desktop acquired admission, connection-owned closers, native
  sender vault and independent sender-death proof, packet FD nonblocking
  transport, bounded payload, backpressure waits and HUP. IPC malformed acquired
  replies are compensated in the lower manager before public projection.
- R2-28–30: Android full SDK/API signature gates, owned subrate request/status
  route, encryption event/snapshot split and receiver cleanup, scan settings and
  normal owned batch ingress. Subrate status is not a measurement; event-only
  encryption is not cached into a replacement connection snapshot.
- R2-32 and R2-34: both DispatchRadio variants delegate parameter reads; packed
  qualifier calls the installed public host factory/provider/native route in
  CJS/ESM, and CI runs Node/Bun on all three desktop operating systems. Its native
  controlled radio remains explicitly synthetic.

## Completed cross-layer reread

The final reread follows desktop/native/RN/IPC graph serialization through strict
record guards: nullable facts and inclusion occurrence references survive to
public snapshots. BlueZ maps native Includes to actual service-object handles
and brackets publication with the authenticated GATT-ready token. Acquired
admission uses the common FIFO and lease, publishes only under current connection
and database generations, and retains failed cleanup in the registry and sender
vault. Directory reads fence manager/daemon generations and validate references
before native effects. Capability registration uses runtime core answers and
keeps the Apple prerequisites specific to Apple.

The reviewed regressions cover public readiness byte/FIFO/invalidation/source
failure, split scan facts and retention, public NAPI parameter read/watch and
source gaps, actual RN method shapes, directory ownership, acquired transport
conflicts/cancel/HUP/generation change, Android SDK/API admission, and WinRT
options. The parameter TCK now invokes callable descriptors and requires a
measured, generation-bound successful result; unsupported descriptors retain
honest non-invocation. The packed qualifier joins its actual installed public
factory/provider/native route, with all three OS lanes and both module formats.

R2-01 requires the combined canonical gate on the frozen head. The prior Windows
recording failure occurred in setup before count/loss/order assertions. Retained
same-budget setup instrumentation and checkpoint-345 CI execute those assertions
successfully; they do not establish the old timeout's root cause, and this audit
does not label it harmless or alter its budget. Final CI remains required.

All original IDs R2-01 through R2-35, including the baseline-fixed R2-10/R2-21,
remain in the remediation ledger. The additional IPC control identity/renderer
clock change remains in scope. The shared worktree also received a Tauri opening-read correction during this
reread. It replaces unbounded opening-time reconciliation with child tickets
that retain the parent absolute deadline and propagate cancellation without
settling the parent watch request. Its new regression initially awaited a signal
before polling the future that sends it; the fixture now polls operation and
cancellation together under one bounded timeout. This correction, its fixture,
docs and the directory/discovery-lifetime batch form the source candidate for
the combined verification. This is a source verdict, not proof
that the outstanding live tests or final gates pass.

## Outstanding execution

The stopped preflight is not a pass. After all fixes/docs are collected and
source reread is complete, run one combined owning build/gate pass and the
remaining affected Linux/Windows live scenarios. Preserve failed attempts and
hardware/OS limitations, and integrate into existing PR #251 only.

## Follow-up after the f0d0a3e8 combined gate

The canonical candidate workflow passed, but live Windows Bun discovery and
cleanup failed. This reopens the discovery-retirement path; the earlier source
reread is not a verification receipt for it. The follow-up retains the exact
native query without calling WinRT Cancel, whose Canceled status did not prove
that characteristic initialization had released its service lock. Public abort
or deadline still ends the caller's wait. Pending native work refuses explicit
disconnect, rediscovery and replacement before any service close; natural
completion permits retry. The new regression covers repeated pending cleanup
and subsequent completion. The follow-up reread follows admission before native await, interrupted-guard
retention, repeated retirement checks, the pre-close barriers in disconnect,
rediscovery and replacement, structured Windows cleanup classification, and
central retained cleanup debt. All four native discovery kinds share this
policy. The original public budget remains unchanged. The portable test imports
the actual registry on every desktop host rather than a separate implementation.
Verification and live replay remain pending; this correction addresses premature
close and does not claim to repair the underlying Windows descriptor delay.
No complete-closure claim is made.

## Final RN readiness review follow-up

The latest PR feedback identified stale initial-probe ordering and erased branded
connection identities in the RN readiness watch. The new regression holds the
actual production-factory probe, drains a newer native readiness edge, and then
releases an older probe answer. The provider now retains the latest state while
opening, emits the initial sample first, then emits that state. Identity fields
remain branded from the backend connection through both observations. Generation
filtering precedes buffering; source failure and connection loss remove the watch
and abort opening, so retained state cannot publish afterward. The native FIFO
write route is unchanged. Collected local verification passes lint/typecheck, all
430 package suites / 5,415 tests, and owning prepack with generated documentation
and package-artifact checks. Clean preflight and final PR CI remain required.

The final automated follow-up reread verifies both Tauri opening gap branches:
receiver re-subscription precedes the bounded fresh probe, so retained pre-gap
records cannot supersede it; post-boundary records remain available. IPC parameter
and readiness guards both require finite nonnegative timestamps and positive
safe-integer ordinals before bounded-stream admission and owned terminal cleanup.
The new broadcast-gap and 18 malformed-metadata regressions precede implementation.

The collected gate exposed the earlier serialization boundary for NaN/Infinity:
renderer event byte accounting now preserves that protocol error as an event-source
terminal, acknowledges the invalid host event and lets the manager retire owned
watches. The same 18 metadata regressions cover both the serialization boundary
and per-stream guards, including the absence of an uncaught transport callback.

The final malformed-event control also covers the dead-pump route boundary:
only existing cleanup commands remain routable under a still-active renderer
lease. Ordinary operation/admission routes stay rejected, and a released lease
cannot route cleanup. Source terminalization thus cannot disable its own teardown.

The complete collected preflight at `10589aee` passes 430 suites / 5,433 package
tests plus all remaining Linux package and Tauri gates. The source freeze is
retained; final evidence reconciliation does not rerun radio scenarios or native
builds. Required current-head PR CI supplies the cross-platform release decision.

## Post-candidate automated feedback

The final candidate review identified two additional issues. IPC parameter and
readiness validators now narrow every required field with assertion-free `in`
guards before reading it. Existing malformed-observation controls cover the
unchanged numeric and terminal behavior. Apple service invalidation now retires
only callback reservations belonging to the exact invalidated service objects,
including descriptors indexed by their admitting service. Unaffected native
work remains owned until its callbacks drain. Once invalidated obligations are
gone, the existing finish path retires cancellation debt and permits a new
discovery; old same-UUID object callbacks cannot consume its reservations. The
Apple executable harness checks these production methods with CoreBluetooth
mutable fixture objects, explicitly as deterministic evidence.

The subsequent scan pump review confirms that matcher exceptions previously
ended delivery without their normalized cause. The pump now carries existing
backend errors unchanged and explicitly normalizes unexpected JavaScript
failures. A public manager regression exhausts all 4,096 live evidence peers
without overflowing the input or delivery queues, checks the original
`stream.quota` operation on the terminal and verifies one owned stop.

The current-head Windows Tauri failure in
`process_shutdown_retains_native_claim_and_retry_owner` exposed test admission
synchronization: a central ingress wake was treated as continuation-outbox
admission. The fixture now waits under its unchanged five-second bound for
`queuedData == 1` and no collection error before shutdown. Its original native
release refusal, retry success, exact retained bytes, claim replay and disposed
receipt assertions remain unchanged. Production runtime code is unchanged.

Post-publication N-API parameter/readiness broadcast lag now resets the receiver
to the current tail before reporting a reconciliation gap. Parameter upstream
gap markers use the same boundary. This discards retained pre-gap measurements
that could otherwise follow the provider's fresh reread. The regression invokes
the actual N-API polling methods after 4,097 records overflow their 4,096-record
receivers, verifies old records are absent, verifies later parameter/readiness
observations survive, and checks the separate upstream parameter gap path.

Android already-paired results now capture the runtime security projection once
with the observed bonded fact, rather than constructing the default unsupported
encryption state. Scheduled and immediate fallback callbacks share that captured
answer. Native fixture controls cover API 35, 36.0 and 36.1 runtime profiles,
ensure one callback and no new createBond operation, and preserve event-only
unknown status when no snapshot API exists. The existing native encryption API
controls separately cover available 36.1 encrypted/unencrypted snapshots and
permission refusal; no physical mobile encryption proof is inferred.

Android security receiver registration now distinguishes failed admission from
successfully registered ownership. A failed registration retires callbacks and
retains the handle until explicit cleanup succeeds or Android reports its
documented not-registered IllegalArgumentException. Only that failed-admission
case clears an absent handle; other cleanup failures retain retry debt. Native
controls cover registration refusal followed by absence and successful retry,
uncertain registration with cleanup refusal then successful release and retry,
and cleanup failure after successful registration. No physical-radio claim is
made by these deterministic Context fixtures.
