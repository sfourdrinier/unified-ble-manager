# Strict BlueZ LE GATT discovery

The BlueZ host requires two distinct mechanisms: implemented LE-only bearer
connect/disconnect under `connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner }`,
and the version-1 `org.unifiedblemanager.LEGatt1.GetSnapshot` extension for
authoritative LE GATT readiness. Verify the current unique owner in trusted host
setup; never copy an example owner or silently accept a daemon replacement.

Stock BlueZ's `Device1.ServicesResolved`, exported objects, MTU and successful
reads are not evidence of successful current LE-specific discovery. Missing or
unknown private API reports `capability.unsupported`. There is no legacy fallback.
Scanning does not require this extension; strict connection/GATT work does.

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
passing its identity to UBM. Reverting a service override does not restore
terminated sessions or guarantee reversal of state-file changes. Physical-radio
tests and an independently reviewed host-specific deployment/rollback procedure
remain separate from the isolated source gates.

## Readiness, invalidation and cleanup

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
