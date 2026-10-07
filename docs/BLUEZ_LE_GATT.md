# Strict BlueZ LE GATT discovery

The BlueZ host requires two distinct mechanisms: implemented LE-only bearer
connect/disconnect under a natively resolved, pinned daemon owner,
and the version-1 `org.unifiedblemanager.LEGatt1.GetSnapshot` extension for
authoritative LE GATT readiness. The shared Rust authority resolves the current
unique owner; an optional `daemonUniqueOwner` policy adds a stricter construction
restriction. Never copy an example owner or silently accept a daemon replacement.

Stock BlueZ's `Device1.ServicesResolved`, exported objects, MTU and successful
reads are not evidence of successful current LE-specific discovery. Missing or
unknown private API reports `capability.unsupported`. There is no legacy fallback.
Scanning does not require this extension; strict connection/GATT work does.

The native authority additionally requires
`org.unifiedblemanager.LinuxAuthority1.GetContract` on the selected adapter,
with the exact three-unsigned-integer reply `(1, 3, 1)` for contract, lease and
GATT observer versions. Missing, malformed or unknown answers refuse lifecycle
admission and keep their native failure details. The maintained source extension
supplies the lease producer and fresh GATT observer together. Installing UBM
alone does not install that derivative daemon.

Initial `when-available` additionally probes the optional revision-1
`LinuxAuthority1.GetLeAvailability` / `LeAdvertisement` observer introduced in
maintained `5.87-ubm.5`. It proves fresh connectable LE advertisement availability,
not GATT readiness, and does not change the `(1, 3, 1)` lease/GATT tuple. Older
daemons retain direct acquisition but report deferred acquisition unsupported.
See [deployment prerequisites](BLUEZ_DEPLOYMENT.md#optional-authoritative-deferred-le-availability)
and [client ownership](NODE.md#linux-initial-deferred-acquisition). Installing or
testing this source is not physical-radio qualification.

Lease revision 3's exact `ReleaseLease` reply is `uttsby` (version, original
token, physical generation, scope, observed-reason presence, raw MGMT byte).
The reason is captured only from the exact generation's native physical-loss
callback and retained in the release answer, so reply-before-signal scheduling
cannot erase it. Absent detail is explicit: presence false and byte 0, never a
reason inferred from the requested disconnect. Reservation/protected scopes
carry no invented physical reason. Older scope-only revision 1 is rejected.

## Upstream feasibility and ownership boundary

The inspected upstream input is the official BlueZ 5.87 archive whose exact
SHA-256 is pinned below. Its `src/bearer.c` implements experimental LE-only
`Connect`/`Disconnect`: the connection selects LE and the disconnect preserves
the Classic bearer. However, `bearer_disconnect` schedules the LE ACL teardown
without tracking D-Bus sender leases. This is **not** an external-client scoped
release mechanism: another application's live LE connection can be affected.
The ordinary `Device1` methods are no substitute; their documented lifecycle
can select another bearer or disconnect all profiles.

The stock `ServicesResolved` property reports the daemon's service view. It
does not provide the successful current LE ATT discovery/error token required
by this package. The retained GATT extension addresses that separate proof,
but it does not add external-client connection leases. Consequently the
extension must therefore implement both mechanisms rather than borrow stock
LE disconnect semantics.
Do not describe the existing private-bus dual-bearer fixture as proof that
stock BlueZ protects another external LE client.

The maintained source includes the authoritative lease mechanism and versioned
native capability handshake. Dual-mode/second-client physical qualification
remains separate: no physical dual-mode peer or second-client test was run during
this source assessment. One inspected host reported stock BlueZ 5.72 and one
adapter; neither its daemon nor its system configuration was changed.

Primary references: [BlueZ Device API](https://bluez.readthedocs.io/en/latest/device-api/),
[BlueZ GATT API](https://bluez.readthedocs.io/en/latest/gatt-api/), and the
[official source archive](https://www.kernel.org/pub/linux/bluetooth/bluez-5.87.tar.xz).

### Sender-scoped lease implementation

The private protocol reserves a sender-scoped, non-recycled token
**before** any physical connection request, then associate the accepted async
connect with that reservation. Cancellation must not erase either the token or
the accepted reply. Cleanup settles the original accepted work, then releases
the exact token; a refused or indeterminate physical release remains owned.

The adapter exposes `ReserveLease(device, reservation)`, `RecoverLease(reservation)`,
`ConnectLease(token)`, `ReleaseLease(token)` and `AckLease(token)`. Reservation
identities are private native nonces shared across one D-Bus sender's sessions,
not application transaction IDs. `RecoverLease` never starts work: it either
recovers the original token or installs an exact cancellation fence before
returning zero. A late original request cannot acquire a resource after that
zero answer. Cross-adapter/device retries cannot retarget an original token.

Release replies retain version, exact token, physical LE generation and scope:
`physical-released`, `reservation-released`, `lease-released-protected`, or
`lease-released-indeterminate`. A matching `lease-released-protected` receipt
retires this logical lease only: the other owner's link stays up, and this
manager records no physical generation or disconnect reason. It still
acknowledges the exact token. Lease revision 3 transfers deferred cleanup to
one daemon-owned obligation for that physical generation before reclaiming
the token. Once the final protecting interest ends, reconciliation runs even
while the original sender remains alive. Disconnect refusals retain that
obligation and a bounded retry; loss or generation replacement retires it.
An ACK never disconnects a foreign-owned or indeterminate link, evicts an
unresolved token, or fabricates a physical-loss receipt. Duplicate ACKs remain
idempotent through exact sender-bound token and reservation fences.
`lease-released-indeterminate`, a protected receipt whose token or generation
does not match, and a protected receipt that carries a disconnect reason stay
retry-owned failures. They are not turned into a successful disconnect. A finished
characteristic or descriptor read or write is not a protected external
interest. A zero-generation
reservation receipt proves no accepted physical work, not that a link closed.
Fresh ATT discovery identity is separate from physical LE generation.

A confirmed token retirement answers a scoped lease question, not necessarily
a physical ACL question. A positively exclusive UBM-created attachment is
eligible for last-owner physical teardown. A preexisting connection, an
in-flight characteristic or descriptor read or write, an active StartNotify
or Acquire, an explicit Connect or Pair, or an indeterminate external
interest is not. A finished read or write is not an external interest: that
hold ends when the method returns or its ATT operation completes. StartNotify
and Acquire last until the sender's bus connection dies; StopNotify does not
clear that hold. The native
receipt must retain that distinction; a retained link is never reported as a
physically closed ACL. Permanent retention of every UBM-created link is not a
substitute for implementing exclusive teardown.

The official 5.87 source hook inventory for this implementation is:

| Hook                                                                     | Required ownership fact                                                                   |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------- |
| `src/bearer.c::bearer_connect`                                           | Distinguish private token-bound creation from stock LE/BREDR requests.                    |
| `src/bearer.c::bearer_disconnect`                                        | No physical release without a current, positive exclusive-attachment proof.               |
| `src/device.c::dev_connect` and `connect_profile`                        | Accepted stock-client interest must protect an existing UBM-created LE attachment.        |
| `src/gatt-client.c` ReadValue and WriteValue                             | A one-shot read or write protects only while its ATT operation is in flight.              |
| `src/device.c::pair_device` and `device_connect_le`                      | Pairing/internal/autoconnect initiation is not automatically a UBM-exclusive acquisition. |
| `src/device.c::device_add_connection` and `device_remove_connection`     | Fence restored/incoming connections and genuine physical loss by attachment generation.   |
| `src/adapter.c::adapter_add_connection` and its connection-event callers | Kernel-restored and incoming peers cannot acquire optimistic UBM-exclusive status.        |

An adapter-level versioned capability handshake must verify the implemented
lease and strict GATT observer protocols under the same pinned owner. Owner
resolution or method introspection alone cannot enable connection capabilities.
Released-token retry history must be bounded and sender-lifetime scoped without
reusing a valid token. Sender death, device removal, daemon replacement, delayed
connect completion, and delayed release replies need production-handler tests.
The producer fixture executes the actual daemon method tables, admissions,
retained records and callbacks on a private D-Bus bus, with device/kernel
boundaries doubled. It covers early cancellation, lost-reply recovery,
sender-death late acquisition, protected/exclusive teardown, asynchronous
disconnect refusal/retry and interleaved sender lifetime acknowledgements.
It is production control-flow proof, not physical dual-mode/second-client proof.

Terminal acknowledgements compact exact sender nonce fences and exact owner token
ranges without reusing identities or treating unseen gaps as canceled. A live
daemon retains at most 1024 unresolved lease records, 128 external sender
interests per peer and 1024 exact fence ranges per sender/owner history. Genuine
fragmentation/counter exhaustion refuses admission or acknowledgement explicitly;
it never evicts unresolved debt. Normal completed interleaved sender cycles are
tested beyond the live-record bound. Keep every allocated native nonce owned
through Reserve or read-only Recover compensation. Sender death retires its
history only after accepted physical effects are reconciled.

## Explicit source preparation

The package includes a source-only patch for the exact official BlueZ 5.87
archive, its original license texts and a hashed source manifest in
[`vendor/bluez`](../vendor/bluez/README.md). Follow that directory's upstream
signature and SHA-256 verification instructions, then use a fresh directory:

```sh
sh /absolute/path/to/vendor/bluez/prepare-isolated.sh \
  /absolute/bluez-5.87.tar.xz /absolute/new-isolated-source
sh /absolute/path/to/vendor/bluez/build-test-isolated.sh \
  /absolute/new-isolated-source/bluez-5.87
```

No daemon is installed or launched by these commands. The tests use production
handlers and isolated private buses, not the system Bluetooth service. This is
compile/protocol evidence, not physical-radio qualification. The extension is
not linked into UBM's native libraries; its derivative source retains separate
GPL/LGPL terms, recorded by the generated SBOM and license inventory.

## Deployment boundary

Use the maintained [deployment owner](BLUEZ_DEPLOYMENT.md) to produce a sealed,
versioned source/binary bundle and reviewed reversible installation plan.
Deployment is an **explicit host action**, not part of installing the npm
package. A trusted operator must separately approve and deploy a derivative
daemon built from the reviewed source, preserve the distribution service's
hardening and Bluetooth state, and arrange a reversible service override rather
than overwrite the distribution binary. UBM never runs privileged installers,
enables daemon features, changes global Bluetooth policy or supplies that approval.

BlueZ 5.87 gates its `org.bluez.Bearer.LE1` methods and properties behind
experimental D-Bus API enablement. The operator must explicitly authorize that
broader API surface and start the reviewed daemon with `--experimental`, then
verify `Connect`, `Disconnect` and `Connected` on the actual device object.
`KernelExperimental` features are not required by this extension and must not
be enabled implicitly.

Do not deploy the isolated-test binary: its compiled configuration and storage
paths point into its temporary source tree. Prepare a separate deployment
build with the distribution's actual paths (normally `/etc/bluetooth` and
`/var/lib/bluetooth`, selected by `--sysconfdir=/etc --localstatedir=/var`),
verify required plugins and shared libraries, and install the verified executable
under a root-owned, non-user-writable versioned prefix. Retain its corresponding
source, patch, build settings and upstream licenses. Preserve service hardening;
do not add capabilities or relax sandboxing to make startup succeed.

A daemon cutover disrupts every client/controller owned by that system service,
including unrelated applications and simulator peripherals. Stop those owners in
a coordinated window, retain the original daemon/service configuration and
state backup, and verify the new unique owner and both required APIs before
creating a fresh UBM manager. Reverting a service override does not restore
terminated sessions or guarantee reversal of state-file changes. Physical-radio
tests and an independently reviewed host-specific deployment/rollback procedure
remain separate from the isolated source gates.

## Readiness, invalidation and cleanup

The derivative daemon also restores acquired server notification sockets after
bonded ATT reconnection while retaining the configured CCC value and descriptor
count. It rearms only the per-ATT `AcquireNotify` path, not global callback-based
`StartNotify`. Repeated connection callbacks and identical writes do not create
duplicate acquisitions. A failed rearm is logged, remains configured, and can
be retried by an identical CCC write; no callback-based fallback is substituted.
CCC disable and disconnect/object teardown invalidate delayed acquisition
replies, and cleanup remains scoped to the original device's ATT identity.
Acquired sockets snapshot their MTU. An actual ATT MTU increase renews only
that ATT's sockets and pending acquisitions with the measured capacity,
preserving CCC counts and rejecting stale replies. Default-MTU clients still
acquire immediately; equal MTU does nothing. No negotiation timer is guessed.
This server lifecycle repair does not change trust, bonds, central routing,
privilege or daemon deployment authority. Its isolated regression is not
physical-radio evidence; qualify the exact deployed derivative separately.

Each new strictly LE primary ATT client performs fresh native GATT discovery
after retiring only that peer's in-memory graph. A matching database hash is
not enough to attest an inherited graph: earlier cache-assisted discovery may
have retained stale characteristics under matching service identities. This intentionally
forgoes the initial cache shortcut for strict LE discovery. It does not delete
cached files or bonds, change trust, reset controllers or modify the Classic
client initialization path. Normal BlueZ persistence can still rewrite the
affected peer's cache metadata. Subsequent native refreshes retain normal hash
and Service Changed handling.

The watcher registers before discovery may publish. Initial pending discovery
has a bounded, cancellable five-second observation wait; it does not initiate a
second native discovery. A graph is published only between identical successful
LE snapshots, fenced by **owner/attachment/revision**. The native errno and ATT
answer remain platform details; timeout does not fabricate a native failure.

LE link acceptance can precede primary ATT attachment. During initial discovery
or recovery, a retired disconnected attachment may therefore still be reported
after the LE link is connected. Under the same pinned daemon owner, UBM waits
for a new attachment only while that LE link is actually connected, within the
original five-second observation budget and the caller's cancellation/deadline.
It never publishes a graph under the retired attachment. Once current discovery
has begun, link loss remains a refusal; a current discovery failure, unsupported
mechanism or malformed answer is not converted into a retry or readiness.

`Invalidated` and old object-removal signals trigger a current reread. They do
not prove failure, and a delayed R1 signal must not invalidate an accepted R2
database. Explicit rediscovery retires old consumers/routing before replacement.
Old notification cleanup retains its original native target, not a newly
resolved same-UUID characteristic. A definite native refusal remains owned and
retryable and blocks replacement publication when it refuses cleanup. A proven
pre-effect enable refusal instead retires only that newly admitted, unacquired
target; ambiguous enable outcomes retain ownership and block retry. A canceled waiter retains the
original accepted cleanup reply rather than issuing another native request.
An indeterminate transport result (`NoReply`, timeout, or an unknown transport
failure) stays fenced: it is not treated as confirmed release or blindly
reissued. Scoped authoritative retirement is needed to retire that uncertainty.
Connection cleanup resolves the exact accepted LE acquisition identity, including
its adapter and daemon owner, independently of discovery or current GATT objects.
It never disconnects a same-address peer through another adapter.
Loss of the registered watcher refuses further GATT admission and retires its
unverifiable databases; it is not mislabeled as a physical Service Changed event.

A snapshot timeout, malformed reply or accepted-identity read failure instead
retires the affected peer's unverifiable database, preserving its structured
cause. It does not poison unrelated devices or claim a physical service change;
explicit rediscovery can reverify that peer after the original cleanup boundary.
Held observation/setup work does not block another peer's values or link events.
Late old failures cannot retire a graph that has since been successfully
reverified, even when the daemon's ready token is unchanged.

The existing Linux private-bus CI lane tests the consumer and daemon extension.
Source tests do not promote a backend evidence label. See the generated
[platform support evidence](generated/PLATFORM_SUPPORT.md) for retained claims.
