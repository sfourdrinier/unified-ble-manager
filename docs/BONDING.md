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

Windows and BlueZ may expose system pairing and durable unpairing where their
public APIs provide it. Android reports public bond-state transitions but does
not ship a reflection-based remove-bond operation. On the current Expo SDK 57 /
Android API 36 artifact, the cancellation capability is also omitted because
the public `cancelBondProcess` API is newer; an aborted or timed-out Android
pair releases the library's pending-operation ownership but cannot cancel the
OS ceremony; it may still reach a later bond terminal state, which `watch()`
reports. Apple and Web keep generic
pairing/bonding unsupported where their public APIs do not provide a truthful
measurement; Web `forget()` remains origin-authorization revocation, not
`unpair`.

See [PLATFORMS.md](./PLATFORMS.md) for current evidence boundaries.
