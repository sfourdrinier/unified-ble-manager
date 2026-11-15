# CoreBluetooth actor ownership: outstanding work

## Current implementation status (2026-11-09)

The delegate queue now owns production connect dispatch, exact native callback
association and queue-shutdown settlement. Discovery retains at most one
current native object for each `(peer, owner)` pairing and bounds all retained
discoveries; a replacement advertisement for the same owner retires the older
native retain. A capacity refusal is surfaced as a typed adapter failure rather
than silently publishing an unusable peripheral. A lifecycle callback retained
for an old owner cannot clear UUID-keyed delegate caches belonging to a current
replacement owner.

Queue teardown explicitly fails active and queued lane callers and retires
their native leases before the queue context is dropped. Adapter-invalidating
states use the same queue-local settlement before they invalidate graph state;
they do not leave a native caller waiting for a callback that CoreBluetooth
will not deliver. A terminal disconnect
does not dispatch its queued successor after lifecycle handoff has failed.
The production disconnect callback now invokes a generic queue-local executor
that owns exact manager/peripheral association, reply settlement, old-owner
lifecycle delivery and successor dispatch. Its identity adapter accepts real
CoreBluetooth objects in production and `Rc` identities in radio-free tests;
tests therefore exercise the same executor without constructing invalid
`CBPeripheral` instances. The executor holds the old cycle until lifecycle
delivery is accepted or retained, reports unassociated callbacks to its
destination, and never starts a replacement after a refused lifecycle.
An unassociated callback becomes a portable `DeviceSourceUncertain` event
with the supplied platform domain, code, message and metadata when available;
it cannot settle or disconnect a replacement UUID owner. Delegate-native
connect and disconnect jobs carry an adapter dispatch generation, so a job
queued before reset or power loss is refused even if power returns before the
actor consumes the state callback.

The focused adapter tests are radio-free and establish only that Rust executor
behavior. They do not constitute physical-radio qualification or prove Apple
will issue every callback in the modeled order. A real `CBPeripheral` must be
obtained from CoreBluetooth for physical validation; hand-allocating one is
invalid and was removed after it raised an `NSRangeException` during teardown.
The latest focused CoreBluetooth suite reports 217 passed, zero failed and one
hardware-dependent ignored test. This count includes the safe executor's
old-owner actor-handoff/replacement-cache regression and remains synthetic
evidence.

## Historical investigation log

Everything below this heading is dated investigation history. It is retained
for evidence and rationale only. Statements there describing a missing lane,
UUID-only callback routing, failing tests, or an earlier pass count are
superseded by the current-status section above and must not be read as current
implementation status.

## Delegate-owned connect dispatch vertical slice (2026-11-09)

Discovery now obtains the registry owner before publication and remembers the
exact `CBPeripheral` with that owner in the delegate queue's `NativeLane`.
After caller admission, the actor transfers only the request identity, reply
state and registry to a Send-safe delegate-queue job.  That job selects the
remembered peripheral by pointer-identical owner and invokes
`connectPeripheral` only on the CoreBluetooth delegate queue.  It refuses
without a native effect if the owner was retired, the queue is closed, or no
remembered exact peripheral exists.

`didConnect` and `didFailToConnect` now retain their real manager/peripheral
arguments and settle only the lane cycle matched by those identities.  The
original reply state remains inside the cycle, so a replacement UUID owner
cannot receive the old result.  The result receipt is acknowledged before a
failed cycle is retired and any queued successor is dispatched.  A callback
that matches no cycle is delivered to the actor as
`UnassociatedConnectionObservation`; it is not discarded and it is not
misattributed to the current UUID owner.

Offline `cargo check --locked -p btleplug --lib --offline` passed after this
increment.  The radio-free CoreBluetooth suite reported 201 passed, five
failed and one ignored.  The five failures are the pre-existing synthetic
UUID-only/retired-disconnect lifetime tests: they exercise the old public
`send_event(ConnectedDevice|ConnectionFailed)` and held disconnect path, not
the new native callback path.  Disconnect, cancellation, lifecycle replay and
their receipt handoff are still not lane-owned; no physical-radio evidence was
gathered.

## Exact active-owner lookup (2026-11-09)

The isolated native lane can now look up a disconnect cycle using an
executor-supplied owner identity comparison. It selects only the active,
nonterminal cycle, never a queued replacement; the admitted owner's identity
remains available after registry retirement. A synthetic regression failed
when the lookup was disabled and passed after it was restored. The guarded,
offline lane suite passed seven tests (minimum sampled free space
23,868,887,040 bytes).

This is not production integration. The actor still dispatches native calls
outside the lane, the delegate still admits UUID-only connect results, and
retired held lifecycle delivery and exact delivery receipts remain unwired.
The guarded delegate-lifetime suite reproduced the same five failures, with
14 passing (minimum sampled free space 23,452,622,848 bytes). These are
radio-free results; they do not establish physical CoreBluetooth behavior.

## Connect-result receipt barrier (2026-11-09)

The isolated `NativeLane` now tracks an outstanding connect-result receipt
for each exact cycle. Its terminal receipt cannot release that cycle's native
object or start a replacement until the earlier connect result has also been
acknowledged. A receipt for another peer or cycle, or a duplicate receipt,
does not acknowledge it. The executor remains responsible for retaining the
returned result until it has actually been delivered or retained by its
destination; the lane records the obligation, not the payload. Terminal
completion distinguishes an invalid receipt or a pending connect result from
successful completion with no queued successor. New synthetic
regressions cover a native failure followed by disconnect, wrong and repeated
receipts, and independent peers.

This is not production integration. Native dispatch, callback association,
retired lifecycle delivery and actor handoff remain unchanged, and the five
previously reported failures remain open. The tests and source changes have
not been compiled or run: available disk space was below the requested 12 GiB
floor. No radio or physical-device evidence was gathered.

## Disconnect observation error (2026-11-09)

When the current actor receives a native disconnect observation with a
platform error, its disconnect waiters now receive that original error rather
than `Ok`. A radio-free reply-queue regression failed before the change and
passes afterward. The offline CoreBluetooth suite reports 188 passed, five
failed and one ignored; the five failing attribution and retired-handoff
tests listed below are still open. The build was stopped if available space
fell below 12 GiB; its minimum sampled free space was 16,339,107,840 bytes.

This does not implement the manager-owned native attempt lane, preserve an
observation lost before it reaches the actor, or change adapter-reset
settlement. It is not physical-radio evidence.

## Connect pre-dispatch refusal (2026-11-09)

A public connect rejected before native dispatch now marks its admission as
having no native effect, including an owner mismatch and retrieval failure. A
radio-free public-entrypoint regression failed before the change because the
retired owner's reclaim state became uncertain. The post-change test run could
not compile: a concurrently modified, out-of-scope shared test references the
absent `IngressRecord::FailureOccurrence` variant. Thus the fix has not been
verified green. An offline `cargo check --locked -p btleplug --lib` passed
with warnings. All runs used a 12 GiB free-space guard; the minimum sampled
free space during the check was 13,045,063,680 bytes.

This is only a recovery increment. It does not replace a stale actor-owned
native peer, wire the manager-lifetime attempt lane, serialize connects,
associate callbacks with native objects, or recover retired disconnect cuts.
The five previously recorded failures and the other limitations below remain
open; no physical-radio behavior was exercised.

## Disconnect admission and batch settlement (2026-11-09)

The disconnect reply queue now prepares every terminal reply before waking
any waiter. A radio-free reentrant-waker regression failed before the change
and passed afterward. The focused `disconnect_` run reported 14 passed and two
failed: both failures were the existing retired admitted-disconnect handoffs.

The public disconnect request now carries its originating source owner and a
caller-versus-actor admission token. The actor checks that owner against both
the current registry owner and its retained native peer at dispatch. A
pre-dispatch cancellation can release its operation slot; a request already
claimed keeps its slot until its reply. A refusal before native dispatch is
marked as having no native effect. Two public-entrypoint regressions failed
before this increment, exercising cancellation and old-owner refusal. An
additional regression checks the claimed-cancellation boundary. An offline,
radio-free `peer_source_registry_tests` run passed all 30 tests after the
change. The build was guarded against free space dropping below 12 GiB; one
earlier attempt was stopped at the floor before the successful run.
The subsequent offline `corebluetooth::` run reported 185 passed, the same
five failed, and one ignored. Three failures concern unassociated connect
callbacks and two concern retired admitted disconnect handoffs. The ignored
case requires a real manager and peripheral retrieval.

These changes do not implement the manager-lifetime native-attempt lane.
Concurrent connects, exact manager/peripheral callback attribution, retired
admitted effects and native errors, replacement recovery, duplicate lifecycle
events, reset semantics, and native reentrancy remain unresolved. The guard
still holds the existing registry admission lock across the native call, and
same-owner disconnects still share the actor's UUID-indexed reply queue. The
results above are synthetic and do not establish physical-radio behavior.

## Disconnect reply retention increment (2026-11-09)

The actor now retains up to 256 disconnect replies for its current peripheral
instead of overwriting the previous reply. The first request dispatches native
cancellation; subsequent requests wait for the same actor-observed terminal
event. A full queue returns an explicit capacity failure. Callback settlement
and actor failure drain every admitted reply in order, and each waker runs after
its reply mutex is released. A retry can initiate cancellation after settlement.

The new radio-free regression initially failed to compile because the reply
queue did not exist, and then passed; a second test checks the bound and a
reentrant waker. The offline CoreBluetooth suite reports 181 passed, five
failed and one ignored. The five failures are the same three unassociated
connect callbacks and two retired admitted disconnects. A process-group disk
guard stopped the command if free space fell below 12 GiB; approximately
13.4 GiB remained afterward.

This increment does not implement the requested queue-confined native attempt
executor. In particular, it does not bind the disconnect replies to an exact
manager/object or a retired owner, preserve a queued lifecycle destination,
serialize connect attempts, route native callbacks, or prove cancellation debt
and reset fences. The existing connect dispatch guard can still refuse a
replacement. The new tests exercise the reply queue in isolation and do not
establish native dispatch, physical-radio behavior, or actor-replacement safety.

## Connect dispatch owner guard (2026-11-09)

The actor now holds the registry's admission lock through the native connect
call and checks that both the current registry owner and the actor's retained
native peripheral owner are the request's owner. A mismatch returns
`connect-owner-retired` after releasing the lock. A radio-free test checks the
mismatch, matching admission, lock exclusion and retirement; its initial red
was a missing-method compilation failure. This prevents the specific stale
owner check/dispatch race, but a replacement whose actor entry still belongs
to an old owner is refused until that ownership is repaired. This is not the
required serialized native attempt lane and must not be treated as a complete
reconnect solution.

The offline, single-job CoreBluetooth suite reports 179 passed, five failed
and one ignored, with a minimum sampled free space of 16,873,201,664 bytes.
The same three unassociated connect callbacks and two retired admitted
disconnects remain failing. The native callback still discards object identity,
reply slots can still be overwritten, cancellation debt and lifecycle effects
are not durably handed off, and the lock/native call interaction has not been
qualified on a physical radio. The diagnostic discovery-retention fallback
remains unchanged.

## Retention verification and attempt-lane review (2026-11-09)

The offline macOS `btleplug` CoreBluetooth lib suite compiled and ran with one
build job and one test thread. The production-discovery source guard and the
20,000-object isolated lease regression both passed; the suite result was 178
passed, five failed and one ignored. The five failures remain the three
unassociated connect/failure callbacks and the two retired admitted-disconnect
handoffs. The ignored test needs a real manager and peripheral retrieval. A
process-group disk guard sampled a minimum of 16,345,845,760 free bytes, above
the requested 12 GiB floor. These are radio-free results, not physical evidence.

Further source tracing confirms that `connect_peripheral` checks the request
owner against the registry, while the native object and reply slot are in the
separate UUID-indexed `PeripheralInternal`. It does not compare that entry's
owner to the request owner, and the last registry check is not atomic with
dispatch or retirement. The native delegate then reduces connect and failure
callbacks to UUID-only events. The disconnected lifecycle can be held inside
an owner's reclaim gate, which is dropped on owner retirement; its delivery
destination does not thereby become durable. These are source-level paths and
the observed test failures, not a newly completed attempt-lane implementation.
Safe integration still needs exact native-object and owner admission serialized
with dispatch and retirement, cancellation debt, and a durable ordered handoff
of captured replies and lifecycle effects. Dropping UUID-only callbacks would
also discard legitimate results, so it is not a safe standalone fix.

## Diagnostic discovery retention disabled (2026-11-09)

The attempt lane is still unwired, so discovery no longer registers native
objects in the diagnostic registry or attaches object tokens. The callback
token traces were removed because there are no registered objects to look up.
The manager identity check and the isolated object/lease registry remain. The
actor's existing native-object ownership and operation dispatch are unchanged.
This prevents the diagnostic registry from accumulating objects across ignored
discoveries, duplicate UUIDs, scan cancellation, resets, and teardown; it does
not establish a bound on unrelated actor ownership. Before enabling registration
again, integrate exact-object ownership and retirement with admitted attempts,
including cancellation, reset and teardown. No connect callback routing,
attempt fencing or admitted-disconnect delivery is fixed by this change.

The entries below record earlier increments; their descriptions of active
discovery registration and token diagnostics are historical.

## Missing-peer disconnect reply (2026-11-09)

An actor disconnect request whose native peripheral is no longer present now
receives `disconnect-peer-unavailable` rather than leaving its reply pending.
The radio-free regression failed before this change and passes afterward. The
focused offline CoreBluetooth suite reports 176 passed, five failed and one
ignored; the five original callback-attribution and retired-disconnect tests
remain unresolved. The run checked free disk against a 12 GiB floor while it
executed. This change does not wire the native attempt ledger, protect the
existing single disconnect reply slot from replacement, or establish physical
CoreBluetooth behavior.

## Discovery token handoff (2026-11-09)

The production discovery callback now includes its queue registry's exact
native-object token in the actor event. A newly created actor peripheral keeps
that token with the same retained native object; subsequent sightings do not
replace the actor entry or its token. Connect, connection-failure, and
disconnect callbacks can look up an already registered object by both its
manager and native identity for trace diagnostics, without registering an
unknown callback or suppressing its existing event. The lookup rejects a
different object with the same UUID and a retired registry entry in a
radio-free registry test.

This is an identity handoff, not connection-attempt integration. The actor
still uses its native object for retrieval and dispatch; retrieved objects have
no token, discovery does not acquire or retire leases, and the token is not
yet used to select a callback owner or reply. Trace lookup does not constitute
callback admission. The ledger, cancellation, durable held-disconnect
delivery, and the five previously failing regressions remain unresolved. The
discovery path still retains registry entries until queue teardown. No physical
CoreBluetooth behavior was exercised. The focused offline CoreBluetooth suite
reports 175 passed, five failed, and one ignored; the five failures are the
same unassociated-connect and retired-disconnect cases. The five registry
tests pass. The new lookup test's initial red was a missing-method compilation
failure, not an observed behavioral failure.

## Queue registry lease primitive (2026-11-09)

The queue-confined registry now offers individually identified lease receipts.
Retiring one receipt cannot retire another, and releasing the last lease
removes that exact peer/object entry; old object and lease identities are not
reused. The isolated registry test covers duplicate retirement, a wrong peer,
and distinct objects sharing a UUID. Registration and lease acquisition are
still separate, so callers must acquire every owner's lease before retiring
any of them. Entries registered without leases remain retained.

This is not production integration: discovery does not yet retain or retire
leases, retrieval and connect/cancel still use actor-held native objects, and
the attempt ledger is not wired to callbacks or actor replacement. In
particular the new primitive does not resolve the existing callback or held
disconnect failures. No physical CoreBluetooth behavior was exercised.
The offline CoreBluetooth lib run after this increment reports 174 passed,
five failed, and one ignored. The failures remain the three unassociated
connect callback cases and the two admitted-disconnect delivery cases.

## Queue-owned native registry increment (2026-11-09)

The delegate queue context now retains the native central manager observed by
its state/discovery callbacks and registers discovered peripheral objects by
UUID and exact object identity. Process-unique monotonically allocated tokens
identify distinct objects without carrying pointers or aliasing across manager
lifetimes. The queue context releases its registry during the existing serialized context cleanup. The registration
path reports a manager mismatch or exhausted token space explicitly.

This is only the first registry increment. The legacy actor still retains and
receives native objects, owns another manager retain, performs retrieval and
native operations, and sends UUID-only connection callbacks. Tokens are not yet
used by actor requests, callback admission, or the attempt ledger. Retrieved
objects are not registered. Actor replacement does not yet provide native
attempt cleanup or durable reply delivery. Discovered objects remain retained
until manager queue teardown; this needs explicit lease/retirement handling
before a complete production migration, particularly for repeated discovery of
distinct objects. The offline CoreBluetooth suite reports 173 passed, five
failed and one ignored. The five failures are three unassociated connect
callbacks and two admitted-disconnect delivery cases, including the newer
native-error regression. The three new registry tests use fake objects and a
serial dispatch queue; they do not exercise native callback registration or
establish physical-radio behavior.

## Manager-lifetime handoff step (2026-11-09)

`CoreBluetoothManagerLifetime` now owns the native manager, delegate, serial
queue and the detached actor's event receiver, peripheral table and outstanding
actor reply slots. It is created by the production CoreBluetooth thread and is
`Rc`-owned on that thread; this handoff adds no cross-thread transfer of its
Objective-C identities. Existing delegate events still carry some retained
native objects across the queue boundary and need migration. Detaching an actor
returns these resources to the lifetime, and a
subsequent attachment can take them back without closing the queue. Final
manager-lifetime teardown stops scanning and releases the queue. Synthetic
tests exercise this detach/attach path, including preservation of an admitted
disconnect's native error and old-owner ticket followed by a reset event.

This is a structural step, not a completed recovery path. Production still
does not replace a failed actor: `fail_actor` settles actor-owned replies and
the thread drains rejected requests until shutdown. A detached actor's
peripheral table is the old UUID-indexed table, not a queue-confined native
attempt ledger. `native_attempt.rs` is still not wired to production dispatch
or callbacks. Nor do these tests prove that a retired owner's admitted event
is delivered: the actor still rejects tickets for a replaced owner, held graph
effects can still be lost, and reset processing still drains the current map.
Exact-object callback admission, native cancellation debt, delivery/waiter
recovery, reset snapshots and real actor replacement remain necessary. The
queue-confined native identity and Apple callback-order limitations below
remain unresolved. These tests do not use a radio.

The offline CoreBluetooth lib suite reports 170 passed, four failed and one
ignored. The failures are the three UUID-only callback attribution regressions
and the retired held-disconnect regression described below; the ignored test
requires a real manager and peripheral retrieval. The table handoff test checks
that the table allocation and an exact directory waiter survive attachment; it
does not construct a real `CBPeripheral` or prove connection-waiter recovery.

## Disconnect error preservation (2026-11-09)

The native `didDisconnectPeripheral:error:` callback now converts and logs the
NSError at admission, and carries it through the peer lifecycle gate and the
actor handoff. Pending operations captured by the lifecycle cut and actor-owned
pending operations receive that platform error; a disconnect waiter still
succeeds when the disconnect was observed. A repeated held disconnect with a
distinct native error is retained as an ordered observation instead of being
coalesced away. Radio-free regressions cover the held waiter and replay, the
actor handoff, and a later error after an earlier disconnect observation.

This is not native-attempt integration. The current UUID-only admission can
still associate the callback with the wrong owner, and retired held lifecycle
effects can still be lost. The public device-disconnected event has no error
field; the diagnostic log and pending-operation errors do not create such a
public event contract. Manager/peripheral identity, manager-lifetime native
debt, serialized dispatch, durable actor handoff, and the four existing
failures below remain open. In particular, this increment cannot guarantee the
error is delivered to the correct owner across replacement or retirement.
The tests are synthetic, not evidence of physical CoreBluetooth behavior.


## Stale queued connect guard (2026-11-09)

The actor now checks a connect request's captured source owner before native
retrieval and again after retrieval. A request already retired at either check
is settled with `connect-owner-retired` outside the reply lock. The radio-free
regression `retired_connect_request_does_not_retrieve_for_replacement_owner`
first failed by reaching native retrieval, then passed with this guard.

These checks do not serialize registry replacement with dispatch: retirement
can still race after the last check, and a replacement request still needs the
manager-owned lane described below. They neither wire the native attempt
ledger nor preserve native callback ownership or disconnect errors. The
focused CoreBluetooth suite now reports 165 passed, four failed and one
ignored; the four failures remain the three unassociated callbacks and the
retired admitted disconnect. No physical radio was used.

## Incremental reset and ledger work (2026-11-09)

The attempt ledger remains **unconnected to production**. It can now retain a
bounded set of disconnect replies on the active cycle, issue cancellation before
the connect settles, and return those replies with the exact native disconnect
error at the disconnect callback. The caller must still deliver those actions;
this does not yet change production disconnect handling.

For an adapter transition initially admitted in an invalidating state, the
delegate now captures each admission-time owner's graph cut, source terminal
reservation and waiters before its barrier replays. It releases those captured
effects after the earlier peer prefixes and does not select a replacement owner
or a later graph at replay. Failure to reserve the cut fails the control path
explicitly. The reset tests also cover an old waiter when a new generation is
installed before replay.

This is partial reset handling: a reset coalesced into an existing overflow
transition captures the owners already in that transition, but does not admit
owners registered after that transition began, and overflow reporting still
fails current registry owners. The actor's adapter-state handler still drains
its current peripheral map. Discovery/service caches are also still cleared
by UUID. These paths are not proven safe for replacement
owners or later generations. The manager-lifetime lane, native identity and
durable callback handoff described below remain unimplemented. At this stage
the disconnect callback still discarded its `NSError`; the later error
preservation step is described above.

The focused offline CoreBluetooth lib run reported 164 passed, four failed and
one ignored. The remaining failures are three unassociated connect callbacks
that can claim a replacement owner and the retired admitted disconnect. The
ignored test requires a real manager and peripheral retrieval. These are
radio-free software results, not physical BLE evidence.

## Previous queue-confined attempt ledger baseline (2026-11-09)

`native_attempt.rs` adds an isolated, queue-confined connection-attempt ledger.
It retains the supplied manager and peripheral objects, associates each
dispatched cycle with its exact owner and reply, and serializes later requests
per peer. A cancelled in-flight connect keeps the lane through a disconnect;
unassociated callback failures retain their platform errors. The ledger is
`!Send` and `!Sync`, and returns actions rather than waking replies while it
is borrowed. Its synthetic tests exercise these state transitions using
reference-counted fake identities.

The focused offline macOS unit run passed seven ledger tests. The existing
delegate-lifetime baseline remained at nine passed and six failed before this
unconnected ledger was added. Those results establish only synthetic ledger
behavior and the continuing failure of the existing handoff paths.

This is **not integrated** into native dispatch or callback admission. No
current CoreBluetooth behavior or existing red regression is fixed by it.
The ledger still needs the manager-lifetime queue and native manager, exact
actor and lifecycle destinations, delivery of its disconnect replies, admitted-effect
delivery and recovery, complete reset snapshots, queue cancellation integration,
retirement cleanup and a fake-native production-boundary harness. Its returned
actions are not durable by themselves. The operational terminal fences are
assumptions for a local lane, not proof of late-callback ordering on Apple
platforms or of physical link teardown. Until those integration obligations
are satisfied, the outstanding warnings and failures below remain applicable.

The connect/disconnect actor ownership migration is **not implemented**. A
connect message retains a typed UUID and its originating source-owner
identity through the native dispatch call. The actor still resolves the native
peripheral by UUID; the request does not retain the native object. The later
owner checks described above narrow, but do not eliminate, replacement races. The
actor still stores the connect and disconnect replies in `PeripheralInternal`.
The delegate emits connect and connection-failure events by peer UUID, and the
actor selects the current reply when it processes the event. An admitted
lifecycle event can be rejected after its source owner is replaced. Adapter
reset replay used to invalidate current registry owners and the actor's current
peripheral map. The delegate-level reset regressions now pass as described
above; actor cleanup and the retired-disconnect regression remain open.

A safe migration needs a manager-lifetime executor and a serialized native
connection lane per peer, independent of source-owner and actor retirement.
Each dispatched cycle must retain the actual native object, owner, reply and
lifecycle destination. Native dispatch, callback-time reservation, epoch
retirement, owner replacement, and future adapter cuts must share an admission protocol.
Callbacks captured before a cut need to retain their exact replies through
queued delivery and recovery. In particular, neither a UUID lookup at replay
nor dropping a cut under an admission, peer, sender, or reply lock is sufficient
for safe ownership and wake ordering. The delegate receives a peripheral
identity rather than a public operation ID; association with a retired or
replacement native operation needs to be established and tested at that
boundary, including late callbacks and reconnects.

Connect and disconnect are only the first operation family. Service discovery,
RSSI, characteristic writes, descriptor reads and writes, and queued
write-without-response payloads also need actor-epoch ownership. Actor graph
state, discovery and activation ownership, lifecycle event destinations,
source terminals and peer waiters must eventually be cut together. Adapter
integration additionally needs admission-time exact participants and cuts,
bounded ordered transition recovery, and identity-checked actor cleanup. The
separate source-terminal reservation failure gap also remains.

The tests required for this work must exercise the delegate and actor handoff,
including registration versus native dispatch, callback versus cut ordering,
caller cancellation, owner and epoch replacement, retired-cut recovery, and
wakers that verify all relevant locks are released. Synthetic results establish
software ordering only; they do not qualify physical CoreBluetooth behavior.

## Previous offline reproduction (2026-11-09)

The earlier offline run failed three focused regressions: adapter replay
invalidated a replacement owner, invalidated a later generation on the same
owner, and lost an admitted disconnect when its owner retired. The current
delegate-level reset results and remaining disconnect failure are above; they
do not establish completed actor ownership or cancellation safety.

The native callback supplies a peripheral object, not an operation ID. A
per-owner epoch recorded only after looking up the current peer by UUID cannot
associate a late callback with a replaced native attempt. Operation admission
must retain the native callback association across retirement, and prevent or
explicitly resolve ambiguous overlapping attempts on the same native object.
Callback-time reply capture must then travel through the existing handoff and
retirement paths; merely adding an epoch field to the queued UUID event leaves
the pending reply and lifecycle state vulnerable to replacement.

## Native connection fence limitation

The delegate receives the central manager and peripheral objects, not an
attempt identifier. Retaining and comparing both object identities can reject
an unassociated callback, but cannot distinguish an arbitrarily late callback
from an older cycle once a new cycle on the same objects has been dispatched.
The supplied native-attempt design identifies no documented CoreBluetooth
ordering guarantee that resolves every cancellation race. A future executor
must retain ambiguous native debt and use only supported terminal observations
as its local fence; it must not infer a callback's operation identity from an
internal counter. A local fence also does not prove that the physical link is
gone system-wide. If a required terminal callback never arrives, a later
request can remain queued while its caller is free to cancel its wait.

The current adapter creates its own native manager and registry. Until there is
coordination at a common owner, a connection-lane guarantee applies only within
one such manager. Neither the manager-lifetime executor nor this fence is
implemented by the current code. Fake-native tests would establish software
ordering, not the undocumented platform ordering or physical behavior.

An offline run on 2026-11-09 of
`unassociated_connect_callback_must_not_claim_replacement_owner` failed:
the replacement owner acquired one callback ticket instead of zero. This is
the existing UUID-only regression, not the requested public-connect and
fake-native actor-replacement test, which remains to be implemented.

## Admission investigation (2026-11-09)

`connect_peripheral` and `disconnect_peripheral` overwrite their respective
actor-owned reply slots immediately before native dispatch. The public
`own_reclaim_operation` task survives caller cancellation, but its operation
counter is neither an exclusive native-attempt lock nor shared with a
replacement source owner. Actor failure can answer those reply slots before a
native result arrives, so task completion is not evidence that native callback
debt has been fenced. The connect and fail-to-connect delegate methods discard
the `CBPeripheral` identity and submit a UUID event; disconnect selects a
source owner by UUID as well. The actor's ticket check then rejects a retired
owner before delivering its event, while its reset handler operates on the
current peripheral map.

The regression `unassociated_connect_callback_must_not_claim_replacement_owner`
exercises the production UUID-only delegate handoff after replacing a registry
owner. It does not assert an invented native attempt: it verifies that an
unassociated callback cannot reserve the replacement. It currently fails
because the replacement receives a callback ticket. It is not a native-object
or physical-radio test. The required native-object admission, connect and
disconnect serialization, retained debt, callback/cut ordering, actor-retirement
handoff, and physical-fence behavior are still unimplemented.

## Public connect regression (2026-11-09)

`public_connect_unassociated_callback_must_not_claim_replacement` submits
real public connect calls from an old and a replacement handle, then injects a
synthetic callback at the production delegate's existing UUID-only handoff.
The offline focused test compiles and fails: the replacement owner receives
one callback ticket where zero are admissible. Fixture replies are settled
before the assertion so failure does not poison a live owner's lock.

This is an executable callback-handoff regression, not a fake native
executor: it does not dispatch through the actor, retain a native peripheral
object, test a per-UUID queue, or prove that an old reply is delivered after
actor retirement. The manager-lifetime executor, owner-aware native-object
admission and serialized native debt remain unimplemented. Completing them
requires changing manager/delegate lifetime and native dispatch together with
callback and cut ownership; changing the UUID handoff alone would not be a
safe production fix. The regression intentionally remains red.

An offline `cargo test --locked -p btleplug --lib -- --test-threads=1` run
with the existing cached target returned 306 passed, five failed and one
ignored. The failures were the new ambiguity regression, the three reset and
retired-disconnect regressions above, and the separate shared source-group test
`retry_after_failure_budget_refusal_must_not_capture_a_replacement_generation`.
The ignored test constructs a real CoreBluetooth manager. This is synthetic
software evidence only; it does not establish which CoreBluetooth reset or
teardown events constitute a physical fence.

## Connect request identity (2026-11-09)

`public_connect_requests_preserve_exact_source_owner` exercises two public
connect submissions for one UUID and checks that each message retains its own
source owner, even after registry replacement. This test covers the message
identity only. The message does not carry a strong `CBPeripheral`: the
bindings do not mark that object `Send`, and the request crosses threads. A
safe manager-thread ownership mechanism remains necessary before a request
can carry the exact native object from admission to dispatch. No unsafe
cross-thread wrapper is implied by this identity field.

The actor still selects its peripheral and connect reply by UUID, and delegate
callbacks still discard native object identity. The red unassociated callback
regression remains open; this stage does not establish a connection lane,
safe owner replacement, callback association, or physical-radio behavior.

## Queued public-connect cancellation regression (2026-11-09)

`cancelled_public_connect_is_not_dispatched_when_actor_receives_it` submits
a public connect, cancels its caller before the actor handles the message, and
then passes the message to the production actor with a synthetic no-radio
manager. The original regression failed because the actor reached the radio
lookup. A shared atomic admission now lets caller cancellation and actor claim
race once; cancellation that wins is consumed before lookup, while cancellation
after a claim leaves the detached operation waiting for its reply. The focused
test passes, as do a concurrent admission race and a synthetic claimed-waiter
test. The latter does not call CoreBluetooth or establish native ownership.

The broader dispatched-attempt guarantee remains unimplemented. The actor can
still select an object and reply by UUID, replace or retire its reply slot, and
receive an unassociated callback through the existing UUID-only path. Thus a
claim is not proof that a native object is retained until a real result or
fence, and this change does not establish isolation from replacement owners.
The native-object executor, callback attribution and reset/retirement work
described above remain necessary. These are synthetic software results, not
physical-radio evidence.

## Pre-claim cancellation settlement (2026-11-09)

A cancellation that wins admission wakes the detached operation directly and
settles it as a certain failure without waiting for the actor. If the task has
not yet been polled, it exits before submitting a message; if already queued,
the actor still rejects the message before native lookup. Reclaim capacity is
released in both cases. Cancellation after claim does not settle the operation:
it retains ownership until its reply, and an effectful failure remains
uncertain. The actor's cancellation reply wakes only after its reply mutex is
released. Focused synthetic regressions cover these boundaries and reclaim
admission, including a reentrant waker. They do not establish the separate
manager-lane, native-object replacement, callback attribution or physical
fence guarantees described above.

## Unassociated failure regression (2026-11-09)

`unassociated_connect_failure_must_not_claim_replacement_owner` injects a
failure through the current UUID-only delegate handoff after replacing the
registry owner. In the isolated offline macOS test target it fails: the
replacement receives one callback ticket rather than zero. This complements
the existing success-path regression; it does not model a dispatched native
attempt or establish delivery of an admitted platform failure. The native
executor and the remaining ownership, cancellation and reset work above are
still unimplemented.

## Disconnect waiter event duplication (2026-11-09)

Successful public disconnect waiters no longer emit their own UUID-only
disconnect events. The actor's native lifecycle observation remains the event
source, so coalesced waiters cannot each emit a delayed duplicate after a later
connection. `repeated_disconnect_replies_do_not_publish_extra_lifecycle_events`
exercises two public waiters and a synthetic reply handoff; it does not exercise
native callbacks or establish lifecycle delivery across owner retirement.
The manager-lifetime native-attempt lane and the five failing regressions
described above remain unimplemented.

# Attempt terminal delivery barrier (2026-11-09)

The queue-confined attempt ledger now retains a terminal cycle until its exact
cycle receipt is completed, rather than returning the next native connect in
the same batch as the preceding terminal effect. During that interval, stale
callbacks cannot claim the cycle, queued requests can still be cancelled, and
other peers can progress. The executor must allocate attempt IDs without reuse
for its lifetime and complete a receipt only after its captured effect has
actually been delivered or retained by its destination.

This is a ledger change, not the production attempt lane. The ledger still
does not own the effects after returning them; the caller must retain them
until delivery. It is not connected to native dispatch, the delegate object
registry, actor replacement or lifecycle replay. The existing five production
regressions therefore remain open, as do native cancellation and reset debt.
The change does not establish physical CoreBluetooth callback ordering.

The guarded offline ledger suite passed all 12 tests; its minimum sampled
free space was 13,439,234,048 bytes. The focused delegate-lifetime suite
reported 14 passed and the same five failures (three unassociated connect
callbacks and two retired disconnect handoffs); its minimum sampled free
space was 13,361,602,560 bytes. These are radio-free software results.

## Queue-local registry and ledger composition (2026-11-09)

The delegate queue context now creates a `NativeLane` when it first observes
the native manager. The lane composes the object registry and attempt ledger:
each admitted cycle has its own native-object lease, queued cancellation
releases only its own lease, callbacks require the registered manager and
object identities, and an exact terminal receipt releases the lease before the
next action can be dispatched. Attempt IDs are monotonic for the lane's
lifetime. The lane bounds retained active and queued cycles at 4096.

This is **not production attempt-lane integration**. Discovery and retrieval
do not transfer a leased exact object to this lane; actor connect and cancel
still dispatch directly, and the native delegate methods still use UUID-only
handoffs. The lane does not yet retain or deliver lifecycle effects or replies
through actor retirement, or associate a disconnect request with its owning
cycle. Reset still acts on the current actor table. Its returned actions must
be executed outside the lane borrow, and a terminal receipt must only be
completed once all exact effects are delivered or retained. Nothing here
establishes physical callback ordering or a system-wide link fence.

The guarded offline `native_lane::tests` run passed four synthetic tests
(minimum sampled free space 15,177,703,424 bytes). The guarded
`delegate_executor_lifetime_tests` run still reports 14 passed and five
failed (minimum sampled free space 15,131,602,944 bytes): three unassociated
connect callbacks and two retired disconnect handoffs. These results use no
radio. The new tests were written before the composition, but the free-space
floor prevented running them in a red state; their red/green cycle was not
observed.

A subsequent guarded `corebluetooth::native_` run passed all 23 registry,
ledger and lane tests, with a minimum sampled 15,111,409,664 bytes free.


## Callback observation lease prerequisite (2026-11-09)

The queue-local native lane can now lease an observed peripheral separately
from an admitted connection attempt. It checks the exact manager, assigns an
opaque existing-registry object identity, bounds concurrent observation leases,
and retires only the exact peer and lease. A callback observation does not by
itself establish an attempt or owner. Observation and attempt leases cannot
retire each other. The lane remains queue-local; no native object is added to a
cross-thread message.

This is a prerequisite only, not the requested delegate integration. The
native callbacks do not call this method or carry its lease, the actor still
uses UUID-indexed reply and event destinations, and a graph-gated lifecycle
can still lose its owner and destination after retirement. The legacy callback
path remains active, including for unassociated callbacks. The remaining work
must bind an exact queued native object and the original actor destination to
an admitted cycle, carry and retire observation receipts through the real
delegate handoff and gate replay, and prove that old effects reach that
captured destination without mutating a replacement. Do not filter callbacks
based on this prerequisite.

A guarded offline macOS run passed 28 `corebluetooth::native_` tests and one
synthetic legacy delegate-handoff test. The first observation-lease test
failed to compile before the methods were added, then passed. Minimum sampled
free space was 49,873,006,592 bytes. These tests do not exercise an actual
CoreBluetooth lifecycle callback, actor retirement, or a physical radio.
