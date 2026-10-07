# Maintained BlueZ 5.87 UBM Linux authority extension

This is an explicitly **UBM-private daemon extension**, not a stock BlueZ API
or a physical-radio qualification. Nothing here installs, launches, replaces or
reconfigures the system daemon. Application integration and deployment require
separate review and explicit host action. Do not silently apply it, enable
experimental APIs, grant privileges or upgrade a host.

The current source deployment identity is `5.87-ubm.10`, with unchanged Linux
authority contract `(1,3,1)`. A finished GATT characteristic or descriptor
read or write holds the link only while that ATT operation is in flight.
Connect, pair, StartNotify, and Acquire hold until the sender's bus
connection dies. Arming auto-connect while probing profiles is daemon
bookkeeping; on an exclusive link this process created, that bookkeeping is
not another application's hold. Exclusive release of an untrusted device also
stops kernel auto-connect, the same gate `Device1.Disconnect` uses. Its
optional revision-1 LE availability observer
implements scan-triggered initial `when-available` acquisition. Installing the
npm package does not deploy this daemon; older `.4` deployments retain direct
LE lease support but explicitly lack the optional availability observer.

The `.4` corrections are retained: it corrects the pinned upstream 5.87 UUID discovery
filter: the upstream call reversed `queue_find`'s callback and match-data
arguments, causing service-filtered discovery to execute an advertised UUID
string as a function. The typed equality callback is covered by an executable
test of the actual daemon filter for matching, nonmatching and empty service
lists. Older deployment receipts remain evidence for their exact older bytes,
not qualification of this corrected daemon.

This revision also fixes the ready-callback lifetime failure reproduced against
`5.87-ubm.3`: pointer-truncated registration IDs could leave a callback queued
after its watch was freed. IDs now remain exact on 64-bit hosts; registration
and device owners retain the watch independently, retirement detaches its device,
and reentrant callback cleanup cannot destroy currently executing user data.
The older `.3` physical receipt retains that crash and is not a pass for `.4`.
Native executable tests prove these boundaries; deploying and qualifying `.4`,
`.5`, `.6`, `.7`, `.8`, or `.9` remains an explicit, separate host action. A receipt for `.4`
is not physical qualification of `.5` or its availability observer. A receipt
for `.4` or `.5` is not qualification of `.6`. The installed `5.87-ubm.6`
daemon returned `lease-released-indeterminate` for the bonded H10 session and
left the link up. The installed `5.87-ubm.7` daemon returned
`lease-released-protected` for that session: the controller had already
initiated the link, auto-connect was armed, and no application interest was
tracked, so the lease adopted it as borrowed. `.8` releases a locally
initiated link when no other application hold remains. The installed
`5.87-ubm.8` daemon's H10 session reported disconnect `released` and the link
was down. A receipt for `.6` or `.7` is not qualification of `.8`. `.9` does
not raise link security when an unbonded LE attribute returns Insufficient
Encryption or Insufficient Authentication. That ATT operation fails and the
ACL stays up. A paired link still retries so an existing key can encrypt it,
and explicit Pair still raises security itself. Installing `5.87-ubm.9`,
the unbonded H10 session completed that GATT exchange, reported disconnect
`released`, and left the link down. The capture had no pairing request. A
receipt for `.8` is not qualification of `.9`. That session does not change
a platform evidence label. Source `5.87-ubm.10` does not treat the controller
initiator bit as proof that this daemon owns the link. A rejected admission
does not keep a hold. An AcquireNotify, StartNotify, or Connect that fails
after early commit drops only that attempt. A StartNotify accepted while GATT
is down keeps that message and drops only that admission if later registration
fails. A dead owner's last in-flight
operation schedules generation-fenced cleanup. Explicit pairing on the same
attachment resumes ATT security retry. Those producer tests do not install a
daemon and are not a physical-radio receipt.

The rc.21 remediation also protects accepted pending Pair admissions, including
Pair opening the link before the first lease. Refusal and cancellation roll back
that admission. Foreign one-shot GATT operations are tracked before a lease peer
exists and protect only their active lifetime. Pre-lease arrival and admission
records retire with their physical generation; adopting a replacement generation
does not adopt an older generation's foreign interest. Deferred acquired-FD
failure retires the exact pending admission and socket/notification resource;
readiness failure, reply-delivery failure and cancellation do not retain a
placeholder that blocks the next acquisition. These corrections are
under verification in `docs/review/RC21_REMEDIATION.md`; they do not constitute a
new installed-daemon or controller receipt.

## Provenance and license

The patch applies only to the official `bluez-5.87.tar.xz`, SHA-256
`26bdcf2cebd7310c6f598850606b037ef0c515fe6608ebc54d22c50c4c32b35f`.
Obtain it and verify its upstream signature independently from
<https://www.kernel.org/pub/linux/bluetooth/>. The patch preserves upstream
copyright and SPDX notices. Daemon code and its tests are GPL-2.0-or-later;
shared GATT client code and the reusable observation-state helper remain
LGPL-2.1-or-later. The repository's package license is **not** substituted for
the derivative daemon's license. Retain/distribute the corresponding patched
BlueZ source and upstream license texts when distributing a derivative binary.
`source-asset-manifest.json` records the pinned archive and patch digest,
mixed source-license provenance and external/source-only distribution boundary.
The retained `COPYING` and `COPYING.LIB` are byte-identical upstream texts,
not the UBM package license. Validate the source inventory with
`node --test vendor/bluez/source-assets.test.mjs`.

## Reproducible isolated preparation and checks

Linux build prerequisites are the normal BlueZ 5.87 development dependencies,
including GLib, D-Bus and libudev development files, a C compiler, make,
pkg-config, and `dbus-run-session`. These scripts do not install dependencies.
For the exact configure flags below, the Ubuntu CI dependency set is
`libglib2.0-dev libdbus-1-dev libudev-dev build-essential pkg-config dbus-daemon
curl patch xz-utils`, on the ordinary CI base image (including its shell,
coreutils, tar and TLS certificate store). Readline, ELL, libical, json-c and
ALSA development packages are not required: client, OBEX and tools are disabled,
and mesh, BTP and MIDI are not enabled. This is a configure/source dependency
analysis, not proof of a newly provisioned minimal container.

The preparation input is the original compressed upstream archive, cached only
under its exact version and SHA-256. A restored archive is rehashed on every
preparation; a cached patched tree or daemon binary is not a substitute. These
scripts do not fetch the archive, so CI must explicitly download/cache that
source input from the manifest URL before running them.
Use a fresh, nonexistent task-specific source directory, not `/usr`, the
running daemon's source, or a radio fixture's files:

```sh
sh vendor/bluez/prepare-isolated.sh /absolute/bluez-5.87.tar.xz /absolute/new-source
sh vendor/bluez/build-test-isolated.sh /absolute/new-source/bluez-5.87
```

The build uses one worker and disk-backed, source-local temporary storage.
It compiles `bluetoothd`, runs the original GATT unit suite and added production
fixtures. The device/lease method-table tests start a **private session bus**, never a
daemon or a system-bus connection. The tests exercise the production snapshot
handler/table, queued Service Changed and failed DB-out-of-sync dispatch paths
(including real cloned clients), initial cached-characteristic replacement and
reconnect through actual native client/server exchanges over an AF_UNIX socket
pair,
full exported-graph checks, callback identity and exhausted counters. The
projection test constructs graph objects in memory; it does not claim a
physical allocation/export-failure injection. No Bluetooth socket, scan,
connection or management command is opened by these tests.
The server-notification fixture includes the actual `gatt-database.c` CCC,
ATT-disconnect, acquired-socket and server-reconnection callbacks. It checks
retained bonded CCC values, socket rearming without duplicate counts, delayed
reply cancellation after CCC disable/disconnect/object removal, failed rearm
retry, independent peers and unchanged callback-based notification behaviour.
Actual ATT MTU-increase callbacks renew acquired sockets with the measured
new MTU; default-MTU acquisition remains immediate. Tests cover equal-MTU
no-op, canceled old replies, peer isolation, refusal/retry and callback
registration/disconnect/free ownership without a negotiation timer.
The actual shared ATT exchange-registration queue-refusal branch is also
executed: it frees the refused registration, not the caller's ATT object,
so disconnect-handler rollback and later ATT use remain valid.
Its ATT and application sockets are AF_UNIX pairs and its application D-Bus
replies are controlled fixtures, not physical or system-bus evidence.
The shared ATT fixture also sends an incoming Service Changed indication over
an AF_UNIX socket to a real server-only ATT owner (the incoming-peer shape when
reverse discovery is disabled). An indication with no registered client handler
receives exactly one Handle Value Confirmation, not Request Not Supported;
this acknowledges the ATT transaction without inventing application delivery or
GATT client readiness. A registered indication handler remains solely responsible
for its confirmation, with a no-double-response control. Reverse-discovery policy
is unchanged. These are protocol tests, not an iOS timeout or link-survival proof.
The clone fixture uses an AF_UNIX socket pair, not a Bluetooth socket. The
device fixture exercises the production pre-allocation policy decision and
registration-failure outcome helper; it does not inject a daemon allocator
failure into full client initialization. The graph fixture includes duplicate
native Include declarations, whose exported unique targets follow stock BlueZ
Includes semantics, as well as missing and stale targets.

## Private authority contract 1, lease revision 3, GATT revision 1

### Optional LE availability observer revision 1

On the selected adapter, `LinuxAuthority1.GetLeAvailability()` returns exactly
`ut`: observer revision `1` and a daemon-epoch monotonic native report sequence.
`LinuxAuthority1.LeAdvertisement(ot)` carries the device object path and its
sequence. The producer emits only actual connectable LE MGMT reports, never
Classic discovery, cached Device1 properties, nonconnectable advertisements or
standalone scan responses. A report does not promise a successful connection.
Signal emission failure is logged rather than silently discarded.

Consumers authenticate the pinned unique sender, adapter path, interface and
signature; register their stream before reading the baseline; and wait for an
exact peer report whose sequence exceeds that baseline. Each pending peer owns
a dedicated D-Bus discovery sender with a `Transport=le` filter, independent of
public scans and other peers. After positive discovery cleanup, the existing
token-bound `LELease1.ConnectLease` owns the actual LE acquisition. Cancellation
and the original caller deadline retire only that sender's discovery work;
accepted start/stop replies and refused cleanup remain owned until reconciled.
No generic Device1 Connect or automatic reconnect is substituted. Missing or
unknown observer versions report unsupported; failed observation reports its
actual platform refusal. Owner replacement invalidates the wait instead of
adopting the replacement daemon's reports.

Executable production-table and private-bus consumer tests cover native report
classification, fresh versus queued/cached reports, owner replacement, absent
observer, cancellation, deadline, concurrent peers/public scan isolation and
cleanup refusal. These are source/protocol tests, not physical-radio receipts.

### Existing lease and GATT authority

The adapter's `LinuxAuthority1.GetContract` verifies the `(1,3,1)` lease and
GATT-observer contract before native lifecycle capability admission. `LELease1`
owns reservation, read-only recovery, connection and exact-token release;
`PhysicalLost(o,t,y)` retains actual physical generation and raw MGMT reason.
`ReleaseLease` returns exactly `uttsby`: revision3, original token, physical
generation, release scope, observed-reason presence, and raw MGMT reason byte.
The reason is retained by the exact physical-loss callback and travels in the
operation's own reply, independent of client signal/reply scheduling. An absent
reason has canonical byte0; reservation/protected/indeterminate receipts never
manufacture an observed physical cause. Revision1 scope-only and revision2
non-reconciling protected-release daemons are refused. Protected logical release
is acknowledged in revision3: the daemon retains cleanup under the exact physical
generation before reclaiming the token, and reconciles when the final protecting
interest ends without requiring sender death. A failed disconnect remains owned
for retry. ACK and physical loss remain separate facts.
The real daemon-table fixture tests retained ownership, accepted late work,
protected versus exclusive release, asynchronous MGMT refusal/retry, exact
cancellation fences and more than 1024 interleaved completed sender cycles.
These are producer/control-flow tests with kernel doubles, not radio evidence.
See [the Linux contract](../../docs/BLUEZ_LE_GATT.md) and
[maintained deployment](../../docs/BLUEZ_DEPLOYMENT.md) for limits and privileged
cutover/rollback boundaries. Normal npm installation never installs this daemon.

`regenerate-source-patch.js /absolute/official-archive /absolute/patched-source`
is the canonical mechanical patch owner. It verifies the pinned archive first,
diffs only maintained source assets against that baseline, and updates the patch
digest in the manifest. Do not hand-edit a generated source patch or its hash.

At the existing device object path, `org.unifiedblemanager.LEGatt1.GetSnapshot`
has no arguments and returns exactly `uttsssiy`:

| Field           | Meaning                                                                                                                 |
| --------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `version: u`    | Exactly `1`; unknown versions must be refused.                                                                          |
| `attachment: t` | Non-recycled, daemon-local primary ATT ownership identity.                                                              |
| `revision: t`   | Monotonic database invalidation revision for that attachment.                                                           |
| `bearer: s`     | `none`, `le`, `bredr`, `mixed` or `unknown`, from accepted ATT socket facts. Unknown destination types never attest LE. |
| `status: s`     | `disconnected`, `discovering`, `ready`, `failed` or `unsupported`.                                                      |
| `errorStage: s` | `none`, `transport`, `discovery`, `projection`, `bearer`, `generation`, `policy` or `registration`.                     |
| `errno: i`      | Zero or a positive native errno; never a Boolean replacement for failure.                                               |
| `attError: y`   | The discovery cycle's own first failing ATT error, if available.                                                        |

`ready` requires successful native GATT initialization/refresh on a live,
exclusively LE ATT transport and a complete, matching exported graph. A
new strict LE primary client first retires only its peer's in-memory graph and
performs full native discovery, even with a matching database hash. This heals
inherited graphs that earlier cache-assisted discovery incorrectly accepted;
it never deletes cached files or bonds. Normal peer-cache persistence remains
active. The Classic constructor and subsequent
native hash/Service Changed refresh semantics are retained. Initial strict LE
discovery intentionally forgoes the native cache shortcut. Existing BlueZ
`Device1.ServicesResolved`, MTU and positive characteristic reads are not
substitutes. Classic or mixed ATT transport explicitly refuses this LE
attestation; an independent Classic ACL without Classic ATT does not itself
invalidate LE evidence. Unsupported export policy or a truncated/stale graph
reports a projection failure rather than returning a partial current database.
Characteristic projection matches the declaration handle and current database's
value-attribute identity, as BlueZ's production exporter does; a matching handle
on a stale database is not accepted. Regression fixtures use that real layout.

Valid Service Changed indications invalidate before dispatch, including queued
ranges. DB-out-of-sync invalidates before asynchronous hash validation. A
cycle retains the first failed result across its queued work; a later success
cannot erase it. Refresh accounting includes cloned-client refreshes.
A synchronous queued dispatch refusal
settles the remaining accepted queue as failed rather than leaving a pending
cycle without a completion callback. Disabled discovery policy and failed
callback registration report explicit refusal, not nonexistent pending work.
Initial completion, refresh observation, cleanup and physical
loss are scoped to the current client/ATT attachment. Counter exhaustion fails
closed. Removing/recreating a device does not recycle the daemon-wide ATT
identity. The daemon's unique D-Bus owner is still required to bound all tokens
across daemon replacement; these numbers are not persistent identities.

The `Invalidated(tt attachment, revision)` signal is a **re-read trigger**, not
a success/failure receipt. It may also be emitted at completion. Authenticate
its exact daemon unique sender, path, interface and signature. Subscribe before
the first snapshot. An integration must read a `ready/le` snapshot with
nonzero counters and zero errors, enumerate the complete graph through the
same pinned daemon owner, then read the identical ready token again before
publishing. Missing/unknown/malformed API, owner change, pending/failed status
or token change refuses publication. A signal queued before a newer accepted
ready token must not later evict that token: fence the actual consumer commit
and invalidation by owner/attachment/revision, not only the watcher send.

## Scope and deployment limits

The patch observes and validates the existing shared GATT pipeline; it does
not split that pipeline into separate Classic and LE clients or alter central routing.
It also repairs server-side acquired-notification lifetime: retained bonded CCC
configuration rearms the lost per-ATT `AcquireNotify` socket after reconnection.
The configured descriptor count is retained, not incremented again. Repeated
connection callbacks and identical CCC writes do not duplicate an active or
pending acquisition. Failed rearming retains the CCC/count and may be retried
by a subsequent identical CCC write; it logs the actual refusal and never
substitutes the callback-based `StartNotify` path. Ordinary initial acquisition
retains BlueZ's existing fallback. CCC disable and disconnect/object teardown
fence delayed replies, and per-device cleanup cannot close another peer's IO.
The callback-based notification path and bonded CCC persistence are preserved.
Acquired FDs carry an immutable MTU. An observed ATT MTU increase renews only
that ATT's acquired-notification sockets and pending acquisitions, keeping
CCC configuration/counts unchanged and fencing stale replies. No exchange is
required for default-MTU clients: they acquire immediately. MTU renewal uses
the existing ATT exchange observer and never guesses readiness or negotiates
on behalf of the application. A failed renewal retains retryable CCC ownership
and never falls back to the callback-based notification path.
It does not change Connect/Disconnect, privilege, policy, trust or global radio
settings. Strict LE acquisition/release still uses the separately implemented
stock LE1 lifecycle under explicit owner attestation. A supported, reviewed
derivative daemon and its exact private API are an explicit deployment
requirement for this discovery route. Stock 5.85/5.87 must not be described as
exposing this extension. Compilation and private-bus tests cannot promote
CoreBluetooth, WinRT, BlueZ or mobile physical evidence labels.
