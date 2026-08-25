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
auto-accepted; passkey and PIN *entry* ceremonies are rejected, matching the
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

This is a deliberate cross-backend difference callers should handle: where a
backend can cancel an in-flight pairing (BlueZ, WinRT), an aborted or timed-out
attempt resolves `{ outcome: 'cancelled' }`; where it cannot (an Android build
without the cancellation extension), the attempt rejects with
`capability.unsupported` instead of misreporting a cancellation that did not
happen.

There is a second, narrower window: an abort or deadline that lands after the
bond has *already* completed but before `pair()` has returned. On BlueZ this is
reported truthfully as `paired` (never `cancelled`), because the native pairing
call resolves only on a completed bond. On Android and WinRT the public outcome
is currently committed when the abort fires, before the native result is
observed, so this race can surface `cancelled` for a peer that did in fact
bond; the bond is still authoritative and visible through `watch()`/`state()`.
Aligning Android and WinRT with BlueZ's report-the-bond behavior is tracked
separately.

Apple and Web keep generic
pairing/bonding unsupported where their public APIs do not provide a truthful
measurement; Web `forget()` remains origin-authorization revocation, not
`unpair`.

See [PLATFORMS.md](./PLATFORMS.md) for current evidence boundaries.
