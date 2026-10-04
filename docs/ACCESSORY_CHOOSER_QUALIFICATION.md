# ASK / CDM physical qualification checklist

This is a prepared procedure, **not a receipt**. It does not authorize an
installation, association removal, phone interaction, force quit or reboot.
Retain one exact source/native-build/package identity for the run. Do not repeat
unrelated background campaigns after documentation or metadata changes.

## Reference app preparation required before using a phone

Use the current Expo reference app and its ordinary `createExpoBleManager()`
factory. RN and Expo share the same public composition; do not bypass `choose()`
with a private native call or build a second radio manager. Refresh native
artifacts through their canonical owner, then build the actual signed consumer.
Retain the binary identity and inspect the built application Info.plist/merged
manifest, not just a source configuration object.

For the simulator's advertised `SIM Polar H10` prefix, the iPhone app needs:

```xml
<key>NSAccessorySetupKitSupports</key><array><string>Bluetooth</string></array>
<key>NSAccessorySetupBluetoothServices</key><array><string>180D</string></array>
<key>NSAccessorySetupBluetoothNames</key><array><string>SIM Polar H10</string></array>
```

A manufacturer-prefix scenario additionally needs the **actual** simulator
company ID in `NSAccessorySetupBluetoothCompanyIdentifiers`, encoded as the
documented hexadecimal string; capture its advertisement before choosing a
prefix. Do not assume arbitrary bytes absent from that advertisement. The
reference consumer declares `006B` (Polar company 107) to match the stock H10
simulator profile; this allowlist is consumer configuration, not radio evidence.
The consuming app still needs Bluetooth usage descriptions, `bluetooth-central`
background mode and the existing stable restoration identity/native continuation
configuration. ASK needs iOS 18+, and this name-prefix selector needs 18.2+.
The newer ASK-qualified relaunch cases require iOS/iPadOS 26+.

Apple also accepts **service-only** and **company-ID-only** filters. A service
filter needs its matching `NSAccessorySetupBluetoothServices` declaration; a
company filter needs its matching `NSAccessorySetupBluetoothCompanyIdentifiers`
declaration. Neither requires a name or manufacturer-data prefix. Those are
optional additional constraints; a name-only filter and unfiltered selection
remain unsupported by ASK. This follows Apple's
[discovery descriptor requirements](https://developer.apple.com/documentation/accessorysetupkit/asdiscoverydescriptor).
For the affected picker acceptance, run `filters: [{ serviceUuids: ['180d'] }]`
and `filters: [{ manufacturerData: [{ companyIdentifier: 107 }] }]` separately
against the advertising simulator. Record an actual OS selection and subsequent
positive HRS value; native admission or a mock selection alone is not radio proof.
The reference scenario's `choose` command accepts replacement selectors as
`{ "filters": [{ "serviceUuids": ["180d"] }] }` or
`{ "filters": [{ "manufacturerCompanyIdentifier": 107 }] }`. Its matching
“Choose service-only H10” and “Choose company-only H10” presets use these exact
arguments. Replacement `filters` cannot be mixed with `namePrefix`, top-level
manufacturer arguments or `alternativeFilters`, so a default name/service
branch cannot accidentally qualify an identifier-only test. Use the existing
`connect-selected` and `sample-selected-hr` commands afterward on the same owner.

The current `example-expo/app.json` declares these ASK keys through Expo
`ios.infoPlist`; verify them in the generated native app before qualification.
The existing scenario screens and authenticated remote registry expose
`accessory-chooser` with `choose`, `select-authorized`, `selected-adapter-state`, `connect-selected`,
`selected-reference`, `sample-selected-hr`, and `cancel`/`stop`. It:

1. Runs only when the app is active, retains the same scenario-owned manager, and reports
   current system-chooser capability before requesting UI.
2. Calls `manager.choose({ filters: [{ serviceUuids: ['180d'],
localNamePrefix: 'SIM Polar H10' }], timeoutMs: 30000, signal })`.
3. Displays/records the returned source and scoped peer ID separately from GATT
   connection status. Never labels a chooser selection as a scan or OS relaunch.
4. Provides cancel and an explicit connect-selected action, preserving refused
   cleanup ownership and using the existing report/evidence channel. Selection
   retains the original hosted manager without requesting scan permissions;
   Android connect-selected prepares that same host's Bluetooth authorization/readiness
   before connecting. An iOS ASK-origin-authorized selection instead enters the
   library's explicit native connect admission, which initializes its owned
   central and waits for actual native readiness without requesting a global
   Bluetooth grant. Neither path constructs a replacement manager or scans.
   `selected-adapter-state` reads that owned manager's actual adapter state,
   including while connection readiness is pending; it neither prepares nor
   allocates a different manager.
5. After connection, `selected-reference` asks the **same manager's** connected
   peer directory for the exact selected public peer ID. It requires one
   currently connected origin reference. On the Expo iOS host it additionally
   validates the Apple backend ID and UUID-shaped native opaque ID before
   returning `nativePeerId` for the trusted continuation controller; the
   Android host validates its own backend ID and address shape. Other hosts
   return the public reference with `nativePeerId: null`. The query is bounded
   and a missing, stale, ambiguous or mismatched record fails without dropping
   the chooser-owned connection. The initial picker selection's scoped ID is
   never parsed as a native UUID or used as a display-name lookup.
6. `sample-selected-hr({ timeoutMs: 5000 })` subscribes to HRS180D/2A37 on that
   exact discovered database, waits for one positive parsed measurement, and
   reports its actual bytes, delivery, sequence, monotonic timestamp and original
   peer/connection/database identities. Its shared deadline bounds subscription
   admission and value wait. Timeout, cancellation, terminal/error and overflow
   are failures, not successful zero-value receipts. Subscription and iterator
   cleanup remain in the scenario ledger, including late admission or refused
   removal; `stop` retries retained cleanup. This does not use `h10-stream`, which
   would construct another manager and cannot prove chooser-to-notification.

For a positive ordinary setup, run `choose` → `connect-selected` →
`selected-reference` → `sample-selected-hr` → `stop`; keep the real picker decision and the emitted
`chooser-hrs-value` plus cleanup records. These commands use the same UI/remote
registry and manager throughout; the sample alone is not a background receipt.
To reuse an earlier OS authorization, run `select-authorized` instead of `choose`.
It calls the public authorized-peer directory without opening a picker or preparing
the radio and requires exactly one origin-authorized record. If multiple records
exist, the emitted `authorized-peers` list allows an explicit retry with that
peer's encoded `PeerReference`; the app never picks a display-name match or the
first entry. A saved authorization does not prove reachability or an active link.
For native background intake, use only that matching connected origin reference
to declare and execute the existing native standing order before releasing the
chooser owner. An absent reference is a failed handoff, not permission to scan,
guess the peripheral UUID, or infer it from the scoped public ID.

The existing authenticated `choose` command accepts paired
`manufacturerCompanyIdentifier` (integer 0..65535) and `manufacturerPrefix`
(nonempty array of bytes 0..255). They add a conjunctive manufacturer criterion
to the existing service/name filter, never replace it. After capturing the actual
stock profile advertisement and confirming company 107/payload `3f155252`, use
`{ "manufacturerCompanyIdentifier": 107, "manufacturerPrefix": [63, 21, 82, 82] }`.
If the captured advertisement differs, use its actual values; do not represent
the profile or this command example as a received advertisement.

`alternativeFilters` appends OR branches to that default conjunction. Each JSON
branch accepts only `serviceUuids` (string/number array), `localNamePrefix`
(string), and the paired manufacturer arguments above; byte arrays map to public
`Uint8Array` values. Example: `{ "alternativeFilters": [{ "serviceUuids":
["180d"], "localNamePrefix": "SIM Polar H10 Other" }] }`. This permits a
matching second advertised identity while preserving conjunction inside each
branch. The parser checks JSON representation; public `manager.choose()` remains
authoritative for UUID/filter semantics and native capability refusals.

Normal `choose` retains its foreground guard: inactive refusal there is a
scenario-level result, not a native result. The separate authenticated
`probe-native-inactive-refusal` command uses the same arguments and owned public
manager path, requires an observed inactive app state, and deliberately reaches
`manager.choose()` without the normal foreground precheck. Retain its actual
native refusal/domain/code. Active/unknown state refuses before allocation;
an unexpected selected peer is recorded as a failed probe and its manager is
released, never converted into a fabricated refusal. Neither command proves a
physical outcome from scripted tests alone.

These commands are available automatically in the current scenario UI; no
private native command, replacement radio or unauthenticated remote endpoint is
needed. The chooser-to-connect tests cover scoped ID handoff through the real
TypeScript session serializer and a previously unobserved UUID through the real
Rust/UniFFI foreign-radio connect/disconnect route. They remain deterministic
boundary evidence, not an ASK UI or radio receipt.

Keep authenticated development remote commands enabled. Remote execution may
request the foreground system picker, but the human's picker decision must be
recorded as human input, not represented as an automated radio assertion.
An app running only in the background must report foreground refusal.

## Bounded execution matrix

| Scenario                 | Action                                                                                                                                                     | Required observation                                                                                                                                                          |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Apple ordinary setup     | Open the foreground chooser and select the simulator                                                                                                       | Real Apple system UI; one authorized UUID result; successful connect → discover → a positive HRS notification through the same manager                                        |
| Android ordinary setup   | Select simulator through public `choose()`                                                                                                                 | Real CDM LE UI; one association result with connectable scoped peer; same positive GATT sequence; association display label is not fabricated advertisement provenance        |
| Selector fidelity        | Test service+literal prefix and one actual manufacturer prefix; include a nonmatching simulator advertisement                                              | Nonmatching candidate does not satisfy the selection; OR alternatives and conjunction inside one filter remain intact                                                         |
| Cancel / timeout         | Cancel while picker is visible; separately allow a short bounded deadline                                                                                  | Exactly one terminal result; picker closes; no late selected peer; a new choice can succeed without restarting the app                                                        |
| Attachment teardown      | Destroy the manager with picker visible, then create a new manager                                                                                         | Old UI owner retires; old manager cannot allocate another picker; new manager's positive choice/connect still works                                                           |
| Configuration refusal    | Use a separately built undeclared-selector app only if needed                                                                                              | Typed refusal before ASK session/UI, no process crash; do not mutate the qualifying app's retained artifact to manufacture the case                                           |
| Foreground refusal       | Request while the app is inactive using retained development controls                                                                                      | Typed native refusal, no picker allocation; record actual app state                                                                                                           |
| Native background intake | Establish native continuation, background/lock, inject simulator link loss then recovery                                                                   | Native recording contains ordered positive values after reconnect/resubscribe; successful archive commit precedes explicit ACK; no JS wake is required for collection         |
| ASK-qualified relaunch   | After real ASK setup, establish the pending native request, remove the process in the **specific** TN3115 scenario, then cause its matching physical event | OS starts app without a manual launch; native `willRestoreState` and positive intake bound to this artifact; source counters and timestamps distinguish pre/post interruption |

Do not simulate user force-quit with an ordinary process kill and call the two
equivalent. Do not use `devicectl`/ADB manual launch before observing an OS wake;
that would mask the property under test. ASK authorization alone is not a
relaunch receipt. Settings/Control Center Bluetooth and airplane/restart cases
have different Apple conditions; record the exact action and follow
[TN3115](https://developer.apple.com/documentation/technotes/tn3115-bluetooth-state-restoration-app-relaunch-rules)
and [the current background guide](BACKGROUND.md).

Use short event-driven windows (normally up to two minutes after the injected
event); no ten-minute rerun is needed merely for chooser/configuration changes.
A timeout with no wake is an observed failure/inconclusive result with retained
logs, not permission to manually launch and convert it into a pass. Longer
background-duration qualification is a separate requirement and should reuse
valid existing receipts unless changed runtime behavior invalidates them.

## Receipt requirements

Capture device/OS version, exact build/source/package/native identity, simulator
revision and advertisement identity, actual foreground state, picker outcome,
native domain/code for refusal, release receipts, reconnect notification values,
native recording ordinals/loss markers, archive commit and ACK boundary, and OS
launch evidence with no manual-launch substitution. Keep ASK setup, ordinary
restoration, ASK-qualified relaunch and sustained background collection as
separate claims. Simulator radio evidence is not a real Polar-device receipt.
