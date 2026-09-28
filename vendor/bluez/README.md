# Isolated BlueZ 5.87 LE GATT observation prototype

This is an explicitly **UBM-private daemon extension**, not a stock BlueZ API
or a physical-radio qualification. Nothing here installs, launches, replaces or
reconfigures the system daemon. Application integration and deployment require
separate review and explicit host action. Do not silently apply it, enable
experimental APIs, grant privileges or upgrade a host.

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
It compiles `bluetoothd`, runs the original GATT unit suite and five added
tests. Only the method-table test starts a **private session bus**, never a
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
Its ATT and application sockets are AF_UNIX pairs and its application D-Bus
replies are controlled fixtures, not physical or system-bus evidence.
The clone fixture uses an AF_UNIX socket pair, not a Bluetooth socket. The
device fixture exercises the production pre-allocation policy decision and
registration-failure outcome helper; it does not inject a daemon allocator
failure into full client initialization. The graph fixture includes duplicate
native Include declarations, whose exported unique targets follow stock BlueZ
Includes semantics, as well as missing and stale targets.

## Private protocol, version 1

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
It does not change Connect/Disconnect, privilege, policy, trust or global radio
settings. Strict LE acquisition/release still uses the separately implemented
stock LE1 lifecycle under explicit owner attestation. A supported, reviewed
derivative daemon and its exact private API are an explicit deployment
requirement for this discovery route. Stock 5.85/5.87 must not be described as
exposing this extension. Compilation and private-bus tests cannot promote
CoreBluetooth, WinRT, BlueZ or mobile physical evidence labels.
