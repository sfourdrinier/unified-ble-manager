# Maintained BlueZ authority deployment

The source producer's exact lease-handler tests establish readiness for the
maintained deployment owner. Every deployment bundle remains **gated** on a
fresh producer-test build before its separate production build. This is not a
Linux-radio qualification or permission to replace a system service. The
authority contract and retained source are described in
[strict BlueZ LE GATT](BLUEZ_LE_GATT.md).

The current source revision adds an optional authoritative LE advertisement
observer after the `.4` ready-callback lifetime correction. Earlier receipts
remain tied to their original bytes, including failures; they do not qualify
this revision. Source `5.87-ubm.10` records who brought the link up, separately
from the controller initiator bit, so releasing a borrowed connection does not
disconnect another application's link. A rejected admission rolls back only
that pending hold. When a dead owner's last in-flight operation ends, that
generation is reconciled without a further request. After explicit pairing on
the same attachment, ATT security retry is enabled again. Those are producer
tests, not an installed daemon and not a physical-radio receipt. The authority
tuple is unchanged, and the package does not automatically upgrade or restart
a host.

## One explicit deployment owner

`vendor/bluez/deployment/bundle.mjs` uses the exact upstream archive digest,
patch and license hashes from `source-asset-manifest.json`. It refuses a source
manifest without `distribution.linuxAuthorityContract: [1, 3, 1]`. The producer
owner sets that readiness fact only after integrating and exercising the actual
lease handlers; adding the field is not a substitute for those tests.

On a Linux build host with the existing BlueZ development prerequisites, use
fresh, disjoint directories outside the repository:

```sh
node /absolute/ubm/vendor/bluez/deployment/bundle.mjs \
  /absolute/bluez-5.87.tar.xz /absolute/new-bluez-bundle \
  /absolute/new-bluez-build-work 5.87-ubm.10
node --test /absolute/ubm/vendor/bluez/deployment/deployment.test.mjs
```

The owner never downloads source, installs packages, escalates privilege or
starts a daemon. It calls the existing exact-archive `prepare-isolated.sh`
twice: the isolated producer-test tree cannot contaminate the production tree.
The first tree runs `build-test-isolated.sh`; failure stops the bundle. The
second builds the maintained daemon/plugins with production `/etc` and `/var`
configuration/storage paths and a versioned `/opt/unified-ble-manager/bluez/`
prefix. Client/tools/monitor/CUPS/OBEX/manpage flags match the isolated gate;
there is no extra plugin-disable fallback. Only ARM64 and x64 Linux ELF targets
are accepted. No `make install` runs.

The caller supplies any isolated build dependencies. Nondefault `UDEV_CFLAGS`,
`UDEV_LIBS` and tool `PATH` are retained with the other build settings; the owner
does not install missing headers/tools or weaken the daemon configuration.

The bundle retains the upstream archive, corresponding patched source archive,
patch, GPL/LGPL license texts, source manifest, preparation/gate/deployment
scripts, configure arguments/environment, generated `config.h`, configure/build
logs, ELF dynamic dependencies, and binary/content SHA-256 identities. Keep the
corresponding source with any derivative binary distribution; UBM's package
license does not replace the BlueZ source licenses. A content hash detects drift,
not trust by itself: review the source and provenance before accepting a bundle.
The bundle remains `built-not-radio-qualified` after passing these build gates.
Failed builds retain their task-specific work/logs for diagnosis; no existing
source or output is overwritten.

The daemon bundle does not choose a simulator policy. On a dedicated H10
simulator host, follow the persisted `ReverseServiceDiscovery = false` admission
and rollback procedure in [the simulator guide](../tool/h10-sim/README.md#linux-requirements).
This prevents BlueZ from probing protected services on an incoming test central;
it does not disable attribute security, change the package's central behavior,
or authorize applying the setting to a production host.

## Reviewed host cutover, separate from file installation

Installing root-owned files is an explicit operator action. The file owner
never invokes `sudo`, `systemctl`, or a daemon. First capture and independently
review the effective distribution `bluetooth.service`, its current `ExecStart`
arguments and hardening. The supplied unit snapshot must match exactly one
simple, nonempty ExecStart; complex environment/quoting or layered ExecStart
forms are refused and require a separately reviewed host procedure.
Review all existing drop-ins as well: a later-sorting `ExecStart` override can
supersede `90-ubm-authority.conf`. Do not omit an older UBM override from the
snapshot to make admission pass. Retiring another deployment is a separate
approved host action with its own recovery receipt, never this installer's job.
Supply the current arguments as a JSON string array, for example:

```json
["/usr/lib/bluetooth/bluetoothd", "--noplugin=hostname"]
```

Using already-authorized root execution, the explicit file operation is:

```sh
node /absolute/ubm/vendor/bluez/deployment/activate.mjs install \
  /absolute/new-bluez-bundle /absolute/current-service.txt \
  /absolute/current-exec-start.json \
  --allow-experimental --confirm-file-install
```

It verifies bundle identities and the host target, installs under a fresh
root-owned/non-user-writable version prefix and creates only
`/etc/systemd/system/bluetooth.service.d/90-ubm-authority.conf`. The drop-in
replaces ExecStart with the versioned binary, preserves the reviewed arguments
and explicitly enables experimental D-Bus APIs. It does not change the distro
binary, capabilities, sandbox, restart policy or Bluetooth state/configuration.
`KernelExperimental` is not enabled. An existing override/prefix is refused;
partial file installation leaves an auditable retained prefix, never a false
claim of service activation. A drop-in may affect a later independently invoked
reload/restart; creating it is therefore a privileged system configuration
change, even though this owner performs no service commands.

Before a separately authorized cutover, recheck that the effective service has
not changed, verify required dynamic libraries/plugins, coordinate every BLE
owner (including unrelated clients and simulator peripherals), back up the
distribution configuration and `/var/lib/bluetooth` state, and approve the
disruption. Preserve hardening: do not add capabilities or relax sandboxing if
the new executable cannot start. Reload/restart, runtime verification and any
state migration remain the operator's separately approved actions.

## Admission and recovery after activation

### Diagnose prerequisites before attempting recovery

`ubm doctor` without a backend reports `compile-config-loadability`; it does not
verify the running daemon or qualify a radio. Installing the npm package does
not install the maintained daemon. A `connection.authority` refusal means that
the current native owner could not establish the required authority contract:

- A missing-method or interface error retains the D-Bus answer under `platform`;
  verify the deployed binary and experimental API enablement.
- A malformed or unsupported version reply is not compatible merely because
  the daemon is named BlueZ. Verify the exact `(1,3,1)` contract below.
- `platform.domain = ubm-linux-authority`, `platform.code = observation-timeout`
  means the bounded contract observation did not complete; it is not proof that
  the peer is absent or permission was denied.
- Owner replacement requires a fresh manager and fresh authority admission;
  old obligations remain attached to their original owner.

Keep the complete structured error in diagnostics. Never invoke installation
from a renderer or reconnect handler. A trusted deployment owner follows the
reviewed file-install, separately approved service cutover and scoped rollback
steps above; reconnecting cannot repair a missing daemon contract.

Under the freshly resolved unique daemon owner, the selected adapter must
answer `org.unifiedblemanager.LinuxAuthority1.GetContract` with exact `(1,3,1)`.
The lease mechanism is `LELease1.ReserveLease(deviceObjectPath, privateReservationId)` then
`ConnectLease(token)`, with `ReleaseLease(token)` returning exactly `uttsby`:
version, original token, physical LE generation, scoped outcome, observed-reason
presence and raw MGMT reason byte. Only the exact physical-loss callback supplies
that reason; reservation/protected/indeterminate outcomes do not invent one.
An absent reason has canonical byte zero. Outcomes
are `reservation-released` (generation zero, only when no physical effect was
accepted or is in flight), `physical-released` (nonzero generation), `lease-released-protected`, or
`lease-released-indeterminate`; a retired lease is not automatically a closed
physical ACL. A finished characteristic or descriptor read or write is not a
protected external interest. An in-flight read or write, StartNotify, Acquire,
or an explicit Connect or Pair still is. `LEGatt1` separately proves current LE-specific discovery.
An owner lookup or successful introspection is not enough to admit lifecycle
work. Missing/unknown contracts fail closed, without a stock-BlueZ fallback.

The native client allocates its private reservation identity before the first
request and retains accepted reserve/connect futures when their caller is
cancelled. This is not a public application transaction ID. A lost reservation
reply remains owned: read-only `RecoverLease(privateReservationId)` returns
the original sender-scoped token, or a native no-admission fence. Compensation
never reserves a replacement or starts a connection. The authenticated
`PhysicalLost(deviceObjectPath, physicalGeneration, mgmtReason)` observation is matched to
the owned physical LE generation; a stale property hint or old loss must not
invalidate a newer connection or GATT database. ATT attachment identity is not
physical LE generation.
The public loss vocabulary is unchanged; the actual MGMT reason byte remains
available as typed platform detail rather than a fabricated new public cause.

After consuming an exact physical, reservation or protected logical release receipt, the
client acknowledges its daemon record with `AckLease(token)`. A failed or held
acknowledgment remains retryable housekeeping debt, not a reversal of the
observed release. For protected logical release, revision 3 retains deferred
cleanup under the exact physical generation in the daemon before reclaiming
the token. It reconciles when the last protecting interest ends without
requiring sender death. Unresolved and indeterminate ownership is never
acknowledged away. Older lease revisions are refused at admission.
The daemon's bounded registry must support normal
long-lived reconnect cycles without evicting unresolved records or reusing
identities.
New connection admission kicks retryable acknowledgment maintenance without
waiting for it. Bounded cleanup observers share one retained attempt; cancelling
an observer never starts another held native request or cancels its driver.

Create a **fresh manager** bound to that verified new owner. Never rebind old
lease tokens, attachment identities or pending cleanup onto a replacement
daemon. A bus-confirmed unique-owner disappearance (`NameHasOwner` for the
original pinned unique name returns false) retires only that daemon's lease,
reservation and acknowledgment obligations. It supplies no physical disconnect reason
and does not retire local iterator, event-handler or D-Bus match cleanup.
An unresponsive but still-live owner, a refused owner query, or a method error
alone leaves cleanup owned and retryable. Retain original diagnostics;
recovery is not a claim that previous physical resources were released.

### Optional authoritative deferred LE availability

Maintained `5.87-ubm.10` includes optional observer revision 1 on
`org.unifiedblemanager.LinuxAuthority1`: `GetLeAvailability` reads the current
advertisement sequence, and `LeAdvertisement` identifies a fresh connectable LE
report. The client subscribes under the pinned unique owner before reading its
baseline, owns a dedicated sender-scoped LE discovery session, then admits the
existing token-bound connection only after a matching fresh report. Cancellation,
deadline and cleanup failure retain that discovery ownership without stopping
public scanning or another peer's session.

The instantiated Linux backend probes this optional mechanism. `.4` and older
daemons still support direct scoped connection and GATT discovery, but report
`capability.unsupported` for initial `when-available`. Installing the addon does
not update that daemon. The tuple `(1,3,1)` remains necessary for lease authority;
it alone is not proof that the optional observer is implemented.

BlueZ's ordinary `Device1` RSSI, manufacturer-data and service-data changes do
not identify the discovery bearer. With merged discovery filters from other
clients, those observations can represent Classic inquiry as well as LE
advertising. An existing device object, cached data or a fresh bearer-ambiguous
property change therefore cannot prove the LE availability this intent requires.

Unchanged RSSI is not itself the limitation: filtered discovery can emit RSSI
observations without its normal delta threshold, and `DuplicateData` can report
unchanged manufacturer/service data. These mechanisms still do not establish
the event's bearer. See the upstream [Adapter discovery-filter contract](https://github.com/bluez/bluez/blob/5.87/doc/org.bluez.Adapter.rst)
and [device-found event handling](https://github.com/bluez/bluez/blob/5.87/src/adapter.c).
No cache shortcut, polling loop or hidden retry substitutes for the native
LE-specific availability observer. Direct acquisition and an explicitly
chosen application reconnect policy remain separate supported paths; neither
should be relabeled as native deferred acquisition. Existing `.4` direct
connection receipts do not qualify the new observer; producer/private-bus tests
are not physical-radio proof. A privileged daemon cutover and physical scenario
remain separately scoped operator actions.

## Scoped rollback

```sh
node /absolute/ubm/vendor/bluez/deployment/activate.mjs rollback \
  /opt/unified-ble-manager/bluez/5.87-ubm.10-PATCH_HASH_PREFIX/deployment-receipt.json \
  --confirm-override-removal
```

Use the actual prefix printed by installation, not the placeholder above.
Rollback removes only the unchanged, root-owned UBM override whose digest
matches the receipt. A changed override is retained for operator review.
Versioned binaries, corresponding source, original service snapshot and receipts
remain recoverable. The running service is unchanged until separately approved
service actions. Reverting configuration does not resurrect connections, undo
state-file changes or prove physical radio behavior. Qualify the exact deployed
binary with real dual-mode/second-client scenarios separately.
