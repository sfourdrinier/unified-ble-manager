<!-- docs/BONDING.md -->

# Bonding / pairing

The 4.0 API exposes security through `manager.security`, while capability truth
comes from the typed feature registrations of the backend attached to a manager,
never from a host name, static table, or simulated radio. The controlling
contract is [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md).

The terms are deliberately separate:

- pairing is the exchange or authentication ceremony;
- bonding is durable key or relationship storage;
- encryption is protection of the current link;
- authentication describes the known MITM/authenticated level;
- authorization is app or OS permission to use Bluetooth or a protected attribute;
- Android association is a Companion Device Manager relationship, not a BLE bond;
- Web origin authorization is browser permission and is not generic unpairing.

The application façade provides `state(peer)`, `watch(peer)`, `pair(peer)`,
`cancelPairing(peer)`, and `unpair(peer)`. `state()` preserves `unknown` versus
`unsupported`; it never converts a missing measurement into a weaker security
claim. Pairing resolves only after a terminal backend result and reports
`paired`, `already-paired`, `repaired`, `rejected`, or `cancelled`.

Applications must inspect the attached backend's registered feature and its
limitations before presenting a pairing flow. If no supported feature
registration exists, pairing is unavailable; applications must not infer
availability from Android, React Native, Electron, or a test backend.

The default ceremony is system-mediated. A custom `PairingAgent` is accepted
only when `security:custom-ceremony` is advertised. Its challenges contain a
sanitized public peer, an opaque challenge ID, a monotonic deadline, and no
native object. PIN/passkey responses are one-shot, never written to logs or
traces, and late responses after cancellation are ignored. Passkeys are exposed
as strings when supplied by the application so leading zeroes are preserved.

Pairing or encryption prompts that an operating system shows while accessing
protected characteristics remain OS behavior. They do not create a normal
application connection lease. The library does not automatically pair and
replay an uncertain GATT write. Use the explicit `withRequiredSecurity` helper
when the application wants a state check followed by an explicitly authorized
pairing attempt and one callback invocation.

Windows and BlueZ expose system pairing and durable unpairing where their
public APIs provide it.

On BlueZ, `pair()` calls `org.bluez.Device1.Pair` and `unpair()` calls
`org.bluez.Adapter1.RemoveDevice`; the library registers a just-works
(`NoInputNoOutput`) `org.bluez.Agent1` on its own bus so the system-mediated
ceremony can complete without an external agent. Just-works confirmation is
auto-accepted; passkey and PIN _entry_ ceremonies are rejected, matching the
`NoInputNoOutput` capability. The agent is registered (not made the system
default) so it applies to pairings this client initiates. This dispatch and
agent policy are covered by unit tests, but their behavior against a live BlueZ
daemon is a separate capability the first-party backend does not yet prove: the
TCK marks `bluez:pairing-agent` unsupported for exactly that reason (a
deterministic boundary cannot exercise a real SMP exchange), so treat live
bonding as unverified until it is measured on real radio.

Android reports public bond-state transitions but does
not ship a reflection-based remove-bond operation. On the current Expo SDK 57 /
Android API 36 artifact, native pairing cancellation is also unavailable because
the public `cancelBondProcess` API is newer. When cancellation is unavailable
the library cannot stop an in-flight OS ceremony, so rather than claim a cancel
it did not perform, an aborted or timed-out `pair()` fails closed by rejecting
with `capability.unsupported`; a bond may still reach a later terminal state,
which `watch()` reports.

`PairOptions.transport` controls how a system pairing request is dispatched.
The default, `'auto'`, uses the platform's normal pairing selection. On React
Native Android, `'le'` invokes Android's transport-selecting bond operation with
`BluetoothDevice.TRANSPORT_LE`; if that operation is unavailable or rejected,
the request fails instead of silently falling back to the platform default. The
Android API 36 surface has no public per-transport bond-state query. An existing
generic bond therefore counts as `already-paired` for `'le'` only on an
unambiguously LE-only device. Dual-mode, classic, and unknown devices retry the
explicit LE operation and fail closed rather than risk accepting a BR/EDR-only
bond as satisfying the request. The
selector crosses the versioned native boundary, so JavaScript and a stale native
binary with different field layouts reject their handshake before pairing.

This option does not imply equal native APIs on every host. BlueZ and WinRT
pair through BLE-only device objects, so `'auto'` and `'le'` reach the same LE
pairing mechanism there. Apple and Web keep generic pairing unsupported. The
option selects the transport, not the LE pairing generation; use
`secureConnections` for the separate Legacy-versus-Secure-Connections policy
described below.

This is a deliberate cross-backend difference callers should handle: where a
backend can cancel an in-flight pairing (BlueZ, WinRT), an aborted or timed-out
attempt resolves `{ outcome: 'cancelled' }`; where it cannot (an Android build
without the cancellation extension), the attempt rejects with
`capability.unsupported` instead of misreporting a cancellation that did not
happen.

There is a second, narrower window: an abort or deadline that lands after the
bond has _already_ completed but before `pair()` has returned. On BlueZ this is
reported truthfully as `paired` (never `cancelled`), because the native pairing
call resolves only on a completed bond. On Android and WinRT the public outcome
is currently committed when the abort fires, before the native result is
observed, so this race can surface `cancelled` for a peer that did in fact
bond; the bond is still authoritative and visible through `watch()`/`state()`.
Aligning Android and WinRT with BlueZ's report-the-bond behavior is tracked in
[#157](https://github.com/sfourdrinier/unified-ble-manager/issues/157), which
records why it is a contract decision rather than a bug fix: learning the truth
means waiting for the radio, and waiting risks hanging the caller behind a
wedged daemon.

`cancelPairing()` no longer forms its own opinion about that race. It reports
what the cancellation _achieved_ by reading the pairing's own result, so the two
calls cannot contradict each other about one operation: `'paired'` when the bond
completed before the cancellation arrived, `'rejected'` (with the peer's reason)
when the peer refused, `'cancelled'` when the cancellation stopped it, and
`'not-pairing'` when there was nothing to stop. A pairing that _fails_ does not
get an invented outcome - `cancelPairing()` rejects with the same error the
pairing rejected with.

An abort or deadline on `cancelPairing()` itself is admitted before any native
cancellation is issued and bounds the whole wait, acknowledgement and pairing
result included. A timed-out or aborted wait reports `operation.timed-out` /
`operation.aborted` rather than inventing `'cancelled'` from an acknowledgement,
and the in-flight pairing stays owned so `state()`/`watch()` (and a later
`cancelPairing()` that waits long enough) can still observe the pairing's own
result.

Two limits are deliberate rather than overlooked. `'not-pairing'` carries a
narrow race of its own: the pairing can settle between the lookup and the
caller's call, and that instant reports `'not-pairing'`. And `pair()`'s abort
path still reports `'cancelled'` without knowing whether the daemon bonded
anyway, because the alternative is worse: learning the truth means waiting for
the radio, and a wedged or unresponsive daemon would then hang the caller - a
hang the suite explicitly forbids (`cancels promptly while the native Pair call
remains pending`). Truth and promptness genuinely conflict here, and resolving
it needs a vocabulary that can say "cancellation requested, bond state not yet
known" rather than a longer wait. Until then, `state()` and `watch()` remain
authoritative for whether a bond exists.

Apple and Web keep generic
pairing/bonding unsupported where their public APIs do not provide a truthful
measurement; Web `forget()` remains origin-authorization revocation, not
`unpair`.

See [PLATFORMS.md](./PLATFORMS.md) for current evidence boundaries.

## Selecting the LE pairing generation (privileged, Linux/BlueZ)

Some peripherals accept **only** LE Legacy pairing and terminate the link on an
LE Secure Connections pairing request; others accept only SC. `PairOptions`
carries `secureConnections` (`'require' | 'prefer' | 'disallow'`, default
`'prefer'`) to say which you need.

`'prefer'` defers to the platform and is always available. The two **directed**
values, `'require'` and `'disallow'`, are the ones that need this section.

### Why this needs privilege at all

`org.bluez.Adapter1` (BlueZ 5.85) exposes no Secure Connections property, and
`org.bluez.Device1.Pair` takes no parameters — verified against a live daemon,
not inferred. The setting lives behind the kernel management socket's Set
Secure Connections command, which requires `CAP_NET_ADMIN`. There is no
unprivileged route.

**This package never acquires that privilege.** It does not open a management
socket, does not shell out to `btmgmt`, and does not assume it is already root.
A library that silently escalates hands an application capabilities its author
did not choose and cannot audit. Instead the host supplies the operation:

```ts
import { createBluezBleManager } from 'unified-ble-manager/node/bluez'

const manager = await createBluezBleManager({
  pairingGeneration: {
    async read(adapterId) {
      // mgmt Read Controller Information -> 'legacy-only' | 'enabled' | 'required'
    },
    async set(adapterId, generation) {
      // mgmt Set Secure Connections: 0x00 legacy-only, 0x01 enabled, 0x02 required
    }
  }
})
```

Omit it and `'require'`/`'disallow'` keep failing closed with
`capability.unsupported`, which is the unchanged default posture. The
`security:pairing-generation` capability reports what your host actually
supplied, so a caller can tell "this build cannot" from "the peer refused".

### What you are agreeing to

Read these before implementing a controller. They are properties of the kernel
setting, not of this package, and they cannot be designed away:

- **It is adapter-wide, not per-pairing.** While held, _every_ pairing on that
  controller uses the selected generation — including pairings this package did
  not initiate.
- **It outlives the process.** The kernel keeps the value until something sets
  it back. A crash between `set` and restore leaves the adapter changed.
- **It is restored after each pairing**, whether the pairing succeeded or
  failed, and concurrent pairings on one adapter are serialised so they cannot
  corrupt each other's restore value.
- **A controller that never settles wedges pairing on that adapter.** Neither
  `read` nor `set` is given a timeout by this package, because the right bound
  depends on your transport and is yours to choose. A controller that hangs
  blocks every later directed pairing on that adapter, and the peer it was
  pairing stays owned for the process lifetime. **Bound your own mgmt I/O.**
- **An undirected pairing running concurrently is not serialised.** Only
  `'require'`/`'disallow'` take the adapter lock. A default (`'prefer'`)
  pairing that overlaps a directed one on the same controller will use
  whichever generation is held at that moment.
- **A failed restore never changes the pairing's outcome.** A bond that was
  created is reported as created; the restore failure is reported separately.
  Leaving a controller in LE Legacy is a security regression that must be seen —
  but so is telling a caller they are not bonded when they are.

### Evidence

This path is covered by deterministic tests only. It has **not** been verified
against a physical peripheral, and nothing here should be read as physical-radio
proof. The `security:pairing-generation` capability reports `limited` when a controller
is supplied, never `supported`, and that label changes only when physical
evidence says so — not because the code looks finished.
