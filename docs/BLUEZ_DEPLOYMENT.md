# Maintained BlueZ authority deployment

The source producer's exact lease-handler tests establish readiness for the
maintained deployment owner. Every deployment bundle remains **gated** on a
fresh producer-test build before its separate production build. This is not a
Linux-radio qualification or permission to replace a system service. The
authority contract and retained source are described in
[strict BlueZ LE GATT](BLUEZ_LE_GATT.md).

## One explicit deployment owner

`vendor/bluez/deployment/bundle.mjs` uses the exact upstream archive digest,
patch and license hashes from `source-asset-manifest.json`. It refuses a source
manifest without `distribution.linuxAuthorityContract: [1, 2, 1]`. The producer
owner sets that readiness fact only after integrating and exercising the actual
lease handlers; adding the field is not a substitute for those tests.

On a Linux build host with the existing BlueZ development prerequisites, use
fresh, disjoint directories outside the repository:

```sh
node /absolute/ubm/vendor/bluez/deployment/bundle.mjs \
  /absolute/bluez-5.87.tar.xz /absolute/new-bluez-bundle \
  /absolute/new-bluez-build-work 5.87-ubm.2
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

Under the freshly resolved unique daemon owner, the selected adapter must
answer `org.unifiedblemanager.LinuxAuthority1.GetContract` with exact `(1,2,1)`.
The lease mechanism is `LELease1.ReserveLease(deviceObjectPath, privateReservationId)` then
`ConnectLease(token)`, with `ReleaseLease(token)` returning exactly `uttsby`:
version, original token, physical LE generation, scoped outcome, observed-reason
presence and raw MGMT reason byte. Only the exact physical-loss callback supplies
that reason; reservation/protected/indeterminate outcomes do not invent one.
An absent reason has canonical byte zero. Outcomes
are `reservation-released` (generation zero, only when no physical effect was
accepted or is in flight), `physical-released` (nonzero generation), `lease-released-protected`, or
`lease-released-indeterminate`; a retired lease is not automatically a closed
physical ACL. `LEGatt1` separately proves current LE-specific discovery.
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

After consuming an exact terminal physical/reservation release receipt, the
client acknowledges its daemon record with `AckLease(token)`. A failed or held
acknowledgment remains retryable housekeeping debt, not a reversal of the
observed physical release. Unresolved, protected and indeterminate ownership
is never acknowledged away. The daemon's bounded registry must support normal
long-lived reconnect cycles without evicting unresolved records or reusing
identities.
New connection admission kicks retryable acknowledgment maintenance without
waiting for it. Bounded cleanup observers share one retained attempt; cancelling
an observer never starts another held native request or cancels its driver.

Create a **fresh manager** bound to that verified new owner. Never rebind old
lease tokens, attachment identities or pending cleanup onto a replacement
daemon. Retain original failed obligations/diagnostics under their old owner;
recovery is not a claim that previous physical resources were released.

## Scoped rollback

```sh
node /absolute/ubm/vendor/bluez/deployment/activate.mjs rollback \
  /opt/unified-ble-manager/bluez/5.87-ubm.2-PATCH_HASH_PREFIX/deployment-receipt.json \
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
