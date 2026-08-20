# External review of PR #26 — markdown-first 4.x docs

Captured: 2026-08-19  
Branch: `docs/markdown-first-4x-rewrite`  
Reviewed head: `0f48e775093337a69dd1c3ce4a05d9bcc828ff41`  
Current package version: `4.0.0-rc.0`  
Intended follow-on: `4.0.0-rc.1` after this review is verified and addressed  
Source: external review of the open, mergeable PR (20 files, +989/−648; CI completed successfully)

This file preserves the review **verbatim** so later planning and implementation can cite it. Verification status lives in `docs/review/2026-08-19-pr-26-external-review-verification.md`.

---

# Verdict

**Yes—this is a strong first documentation round and a major improvement over `main`.** The branch turns the repository’s front door from an architecture/status document into something a developer can actually navigate. The documentation map, host entrypoint table, method index, migration guide, and explicit explanation of bytes, cancellation, ownership, snapshots, and bounded streams are all meaningful upgrades.

However, **I would not approve PR #26 unchanged**. I would request changes, fix the technical correctness and lifecycle issues below, then merge it. Several snippets are presented as canonical or copy-pasteable but can leak resources, hang indefinitely, fail against valid SIG-compliant peripherals, or teach semantics that the API does not actually provide.

I reviewed the current head, `0f48e775093337a69dd1c3ce4a05d9bcc828ff41`. It remains open and mergeable, with 20 changed files and 989 additions; its CI workflow completed successfully.

My rough assessment:

| Area                                   |   Rating | Assessment                                             |
| -------------------------------------- | -------: | ------------------------------------------------------ |
| Direction and information architecture |     9/10 | Definitely the right direction                         |
| Readability                            |     8/10 | Much easier to approach                                |
| Core mental-model explanation          |     9/10 | One of the strongest parts                             |
| Technical accuracy                     |     6/10 | Several material BLE and lifecycle errors              |
| Copy-paste safety                      |     5/10 | Canonical examples are not exception-safe              |
| Cross-host completeness                |     5/10 | RN is much stronger; Electron, Tauri, Node remain thin |
| Documentation testing                  |     5/10 | Good guardrails, but mostly string assertions          |
| Overall first round                    | **7/10** | Good work, not yet release-quality documentation       |

---

# What became substantially better

## 1. The README now behaves like a product front door

The old README was rigorous, but it opened with package status, architecture authority, evidence boundaries, and host internals. The new README starts with a clearer product description, documentation map, installation path, public entrypoints, an RN construction example, a complete journey, rationale, and method index. That is the right hierarchy for outside developers.

The new sections explaining *why* the API uses `Uint8Array`, `AbortSignal`, generation-bound snapshots, explicit host imports, bounded streams, and awaited cleanup are especially good. Those sections communicate the library’s engineering philosophy without forcing readers through the implementation plan.

## 2. The public entrypoint story is clearer

The host-specific subpath table is useful and accurate in most places:

* Neutral root does not select a radio.
* React Native has its own factory.
* Web has a matched chooser and manager.
* Electron separates main and renderer.
* Node chooses an explicit backend.
* Tauri exposes an IPC-oriented manager.
* Profiles, codecs, testing, backend SDK, and CLI are identified.

That is much better than making users infer package boundaries from `exports`.

## 3. The migration guide is the right artifact

The side-by-side `react-native-ble-plx` comparison is exactly what prospective adopters need. It directly addresses:

* no `new BleManager()`;
* Base64 to `Uint8Array`;
* transaction IDs to `AbortSignal`;
* immortal `Device` objects to connection leases and snapshots;
* async destruction;
* removed functionality;
* method-by-method replacements.

The “Gone on purpose” section is particularly valuable because it explains design intent rather than merely listing breaking changes.

## 4. The documentation is more honest about host differences

The branch correctly states that Tauri returns `IpcBleManager`, not the neutral `BleManager`, and that Electron renderers use a client rather than owning a radio. That distinction matters technically and for security.

## 5. The helper layer is genuinely strong

`scanUntil`, `connectAndDiscover`, `firstNotification`, `collectNotifications`, and `withConnection` are the best public teaching surface currently available. The helper implementation handles partial discovery failure, releases subscriptions, releases connections, and preserves operation errors alongside cleanup failures. That is exactly the behavior the introductory documentation should showcase.

---

# Must-fix issues before merging

## 1. The README’s “complete loop” is not exception-safe

This is the largest documentation problem.

The loop creates a connection, database, and subscription, but cleanup only happens through trailing statements. A failure during discovery, path resolution, reading, writing, subscription creation, decoding, or consumer handling skips connection release and manager destruction. A failure in `subscription.remove()` also prevents the remaining cleanup.

The existing PR review already identified this issue, and that feedback is correct.

There is another structural problem: the notification loop has no stopping condition. Unless the device disconnects or the stream terminates, execution never reaches connection release or manager destruction. The `AbortController` is created but never externally aborted.

### Recommended correction

Make the first README journey helper-first:

1. `scanUntil` finds one peer and owns scan cleanup.
2. `withConnection` owns connection release.
3. `connection.discover` creates the database.
4. `firstNotification` or `collectNotifications` owns subscription cleanup.
5. A top-level `finally` destroys the manager.

The README can then link to a lower-level raw ownership tutorial for users who need persistent subscriptions.

The canonical first journey should be finite. “Subscribe forever, then somehow run teardown after the loop” is not a useful onboarding example.

Also preserve both the operation error and any cleanup error. Throwing a generic cleanup error from `finally` can hide the original failure. The existing `withConnection` helper already models this correctly; the manager-level example should follow the same pattern.

---

## 2. The README loop assumes characteristics that valid Heart Rate devices do not have

The scan accepts any named peripheral advertising Heart Rate Service. The subsequent code unconditionally resolves:

* Battery Level;
* Heart Rate Measurement;
* Heart Rate Control Point.

That is not safe.

Heart Rate Measurement is mandatory and notification-based. Body Sensor Location is optional. Heart Rate Control Point is conditional and exists only when Energy Expended is supported. Battery Service is a separate service and is not implied by Heart Rate Service.

The current review already caught the optional-path problem.

### Recommended correction

The minimal guaranteed HRS example should:

* scan for Heart Rate Service;
* connect;
* discover;
* resolve Heart Rate Measurement;
* subscribe;
* collect one or several measurements;
* clean up.

Move Battery Level and Reset Energy Expended into separate recipes:

* **Read battery when present**
* **Reset energy expended when supported**
* **Handle optional or conditional profile features**

For optional paths, either inspect the snapshot before resolving or catch only the precise `gatt.not-found`/unsupported-property case. Do not catch every BLE error and call the characteristic “optional”; a disconnect or stale generation is not optional-feature absence.

---

## 3. The migration guide contains executable mistakes

The overall migration guide is good, but several examples would actively mislead developers.

### Battery Level is used for both read and write

The sample resolves `batteryLevelSelector()`, reads from it, and then writes `[1]` back to the same path. Battery Level is a read-oriented characteristic. The PR’s existing review identified this correctly.

Use a separate writable characteristic. A profile-specific example could use Heart Rate Control Point, but only after showing that it is conditional. A generic migration example may be better with clearly named application UUID placeholders:

```ts
const readablePath = …
const writablePath = …
```

That avoids suggesting every device exposes a SIG control-point characteristic.

### Notification cancellation is shown after an infinite loop

The guide does this conceptually:

```ts
for await (const item of sub.values) {
  // ...
}
abort.abort()
await sub.remove()
```

But `abort.abort()` cannot stop a loop that must finish before the abort line is reached. Cancellation needs to come from:

* a UI event;
* a timer;
* another task;
* a component teardown;
* a helper such as `firstNotification`.

### The migration sequence contradicts itself

Early guidance says to remove `react-native-ble-plx` and not keep both packages. The later “Suggested order” says to install the new package alongside the old one, migrate incrementally, then remove the old package.

The correct migration rule is:

> Both packages may be installed temporarily, but only one BLE stack may own the physical radio/session at a time.

That gives users a realistic feature-flagged migration and rollback path without encouraging simultaneous managers.

### The scan policy is wrong for a name-dependent predicate

The migration scan uses `duplicatePolicy: 'first'` while matching on `localName`. A name may arrive in a later scan response. The README already switched this scenario to `merged`; the migration guide should do the same.

### Deadlines need explicit semantics

The same absolute `until` value is reused for scan, connect, discovery, read, write, and subscribe. That means the 20-second deadline is a budget for the **entire journey**, not 20 seconds per operation.

That can be intentional, but it must be explained. Otherwise developers will assume each step gets 20 seconds and be surprised when a later operation immediately times out.

I recommend one of:

* `journeyDeadline` with an explicit note that it is shared;
* a fresh `deadline(manager.monotonicNow() + N)` for each phase;
* a helper such as `deadlineAfter(manager, 15_000)`.

---

## 4. The code exposes profile commands that contradict the Bluetooth SIG profiles

This is more serious than documentation wording because it affects the public API.

`standard-commands.ts` exports:

* `readHeartRateMeasurement`;
* `readTemperatureMeasurement`;
* `readBloodPressureMeasurement`.

It also exposes subscription variants.

Heart Rate Measurement is specified as mandatory **Notify**, not Read. Blood Pressure Measurement is mandatory **Indicate**, not Read.

The generic command functions resolve a path and immediately call `database.read`, `write`, or `subscribe`; they do not verify the discovered characteristic properties before selecting an operation.

### Recommended correction before stable 4.0

For standard SIG helpers:

* Remove `readHeartRateMeasurement`.
* Remove `readBloodPressureMeasurement`.
* Verify the exact Health Thermometer requirements before retaining its read helper.
* Keep `subscribeHeartRateMeasurements`.
* Keep `subscribeBloodPressureMeasurements`.
* Add property validation before read/write/subscribe.
* Throw a precise error such as `gatt.property-not-supported` before asking a backend to perform an operation the discovered characteristic does not advertise.

A looser alternative would be naming them `readHeartRateMeasurementIfSupported`, but I would not do that in a module called `standard-commands`. Standard commands should model the standard.

This also explains why the canonical example service currently tries to read Temperature Measurement and Blood Pressure Measurement. The documentation pass has surfaced a real public-API correctness issue.

---

## 5. The React Native example service is not yet canonical-quality

The rewritten service is a substantial improvement over the legacy-style example, but several implementation choices directly conflict with the new documentation’s claims.

### It does not actually demonstrate cancellation or deadlines

Every operation is produced by:

```ts
function operation() {
  return { signal: null, deadline: null }
}
```

Yet its README says the fixture demonstrates `AbortSignal`. That should be fixed either by giving each user action a real controller/deadline or by changing the claim.

### Overflow records are silently ignored

Both scan and notification consumers handle `value` and `terminal`, but not `overflow`. One of the product’s distinguishing claims is explicit backpressure and visible loss. The canonical example must either:

* report overflow to the UI;
* expose it through a callback;
* terminate when loss is unacceptable.

Silently skipping it teaches the opposite behavior.

### Duplicate GATT paths are silently collapsed

`findCharacteristicPath` uses `.find()`, selecting the first matching UUID pair. The rest of the documentation emphasizes that repeated services and characteristics make UUID-only lookup ambiguous. The example should use `resolveCharacteristicPath` and fail with `gatt.ambiguous-path`, or explicitly accept service/characteristic occurrences.

### Optional-profile handling catches too much

`readProfileValue` catches every error and converts it to `{ skipped: true }`. That masks:

* connection loss;
* stale handles;
* permission failures;
* backend failures;
* malformed payloads;
* programming errors.

Only expected optional-feature absence should become `null` or `skipped`. Transport and lifecycle errors should propagate.

### Manager creation can race destruction

If `ensureManager()` has started but not completed, `destroy()` sees `this.manager === null` and returns. The pending creation can then resolve and assign a live manager after destruction has finished.

`destroy()` must await `managerCreation`, then destroy the resulting manager, or mark the service as destroying and immediately destroy a late-created manager.

### Cleanup is sequential and short-circuits

`destroy()` calls:

1. `stopScan()`;
2. `disconnect()`;
3. `manager.destroy()`.

If step 1 throws, steps 2 and 3 never run. The same issue exists when notification removal fails before disconnect.

Cleanup should attempt every owned resource, collect every failure, and then report an aggregate result.

### The example’s identity changes on every manager creation

Both `clientId` and `managerId` include an incrementing number. A recreated manager therefore gets a different `clientId`. That is a questionable example for a system where restoration authorization uses a stable client identity.

Use:

* stable `clientId` for the logical application BLE owner;
* unique `managerId` for an individual manager instance;
* stable `hostSessionScope` for the host-owned scope.

---

## 6. `isBackgroundEnabled` is a misleading public option name

The implementation and docs are technically honest about its effect: it adds a required Android BLE hardware feature. It does **not** enable background execution, create a foreground service, or change lifecycle behavior.

That means the option name is wrong.

On Android, declaring `android.hardware.bluetooth_le` with `required="true"` causes Google Play to exclude devices without that feature. Android explicitly recommends setting it true only when the app cannot work without BLE. The `neverForLocation` flag is also a strong assertion, and Android warns that it may filter some BLE beacons.

Since this is a new 4.0 API without production users, rename it now:

```ts
androidBleRequired?: boolean
```

or:

```ts
requiresBluetoothLeHardware?: boolean
```

Do not carry a misleading compatibility name into stable 4.0.

Two related issues:

* The plugin accepts iOS background mode `peripheral`, while the product describes itself as a BLE central library. Either reject `peripheral` or state very prominently that this only writes an Info.plist value and exposes no peripheral API.
* `BLEPLX_PLUGIN_DEBUG` retains the old product name. Add `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG`; retaining the old variable temporarily as an alias is reasonable.

---

## 7. “Same manager contract on every host” overstates the current unification

The opening description says “Same manager contract on every host.” Later documentation correctly explains that:

* Tauri returns `IpcBleManager`;
* Electron renderers use `ElectronRendererBleClient`;
* Web introduces a chooser;
* Electron main constructs a neutral manager behind an IPC boundary.

Those are not the same public contract.

A more accurate product claim is:

> One bytes-first BLE model and lifecycle semantics across hosts, with host-specific ownership façades.

That is still compelling. It is also defensible.

Longer term, decide whether Tauri and Electron renderer should deliberately converge on the same high-level façade. At present, the unification is strongest at the conceptual and backend-contract levels, not the consumer-method level.

---

## 8. The documentation tests verify wording, not working documentation

The new test names are better, and checking package identity, public exports, historical-document boundaries, and release-channel consistency is useful. But `Docs.consumer.test.js` is overwhelmingly a set of `toContain`, regex, and filename assertions. It proves that phrases exist; it does not prove that examples compile or behave correctly.

The parity test similarly proves that bare and Expo examples contain the same source patterns. It even asserts that profile reads exist, but it does not test whether the selected SIG characteristics are actually readable. Exact source parity can preserve the same bug twice.

### The next documentation test layer should do this

1. Extract `ts`, `tsx`, `js`, and `json` fences from public documentation.
2. Label fences as `compile`, `fragment`, or `pseudo`.
3. Build generated TypeScript harnesses.
4. Typecheck them against the **packed package**, not repository source aliases.
5. Execute finite recipes against `DeterministicTestBackend`.
6. Validate all Markdown links and anchors.
7. Add semantic documentation rules:

   * Battery Level path cannot be written.
   * Heart Rate Measurement cannot be taught as a normal read.
   * Blood Pressure Measurement cannot be taught as a normal read.
   * A name-dependent scan cannot use `duplicatePolicy: 'first'`.
   * A created manager, scan, connection, or subscription must have a demonstrated cleanup path.
   * A newly created `AbortController` cannot have its controller discarded.
   * RN platform selection cannot silently map every non-iOS platform to Android.
8. Generate version references and plugin option tables from source rather than copying them into many files.

Green package CI is valuable, but it cannot catch the semantic mistakes above because the Markdown examples are not compiled or executed.

---

# File-by-file feedback

## `README.md`

### Keep

* Product-oriented opening.
* Documentation map.
* Explicit host entrypoint table.
* “Why the API looks like this.”
* Method index.
* Migration and examples links.
* Honest backend maturity statement.

### Change

1. Put the H1 and one-sentence product statement before the sponsor line. Right now the GitHub preview begins as an advertisement rather than the package identity.
2. Restore a simpler form of the old README’s important distinction:

   * package/API stability;
   * host/backend maturity;
   * physical-hardware validation.
3. Replace the raw “complete loop” with a finite helper-based loop.
4. Move optional Battery and Control Point examples out of the first journey.
5. Replace “same manager contract” with “shared model and semantics.”
6. Add a compact host requirements table before the RN-specific material.
7. Fill the empty `## License` section. The package and repository currently specify Apache-2.0.
8. Link `SECURITY.md` and `SUPPORT.md`.
9. Replace “bound live-radio receipt” with plain language such as “artifact-bound physical-hardware validation.”
10. Clarify Bun support. The README says the package works with Bun, while `package.json` declares Node engine versions. Say “installable with Bun” unless Bun runtime behavior is actually tested.
11. Do not repeat the exact package version manually in numerous pages. Generate or inject it.

## `docs/GETTING_STARTED.md`

This is heading in the right direction, but it promises a first scan/connect/read/notify journey and then sends readers back to the README for the actual loop. It should be self-contained.

Split the RN setup into three explicit paths:

### Expo/CNG application

* Install with `npx expo install unified-ble-manager`.
* Add plugin config.
* Run prebuild.
* Build a development client or production binary.
* Explain that config changes require a native rebuild.

Expo recommends `npx expo install` because it selects compatible versions where possible and warns about incompatibilities. Config plugins require Prebuild support; libraries with custom native code cannot run in Expo Go.

### Existing bare React Native app adopting Expo config plugins

Explain that Expo modules must first be installed and configured, commonly with `npx install-expo-modules@latest`.

### Bare React Native without Expo Prebuild

Document the manual native setup:

* Android manifest permissions and feature declaration;
* Android runtime permissions;
* iOS usage description;
* deployment targets;
* codegen/New Architecture requirements, if mandatory;
* pods and rebuild steps.

Also fix this platform expression everywhere:

```ts
Platform.OS === 'ios' ? 'apple' : 'android'
```

It maps every non-iOS platform to Android. Use an explicit function that accepts only `ios` and `android` and throws otherwise. The example service already does this correctly.

## `docs/TUTORIALS.md`

Reorganize it around safe finite recipes:

1. Find one peripheral.
2. Connect and discover.
3. Read one characteristic.
4. Write one characteristic.
5. Receive one notification.
6. Maintain a long-lived subscription.
7. Handle overflow.
8. Handle disconnect and reconnect.
9. Tear down the application session.

Each recipe should be independently runnable and should clean up all resources it creates, even when the central operation fails.

The primitive ownership version can remain, but it should come after the helper-based version.

## `docs/HELPERS.md`

This is among the best files in the branch. Make it more prominent.

Add one clarification for each helper:

* who owns the connection after success;
* whether the deadline is shared;
* how cleanup failures are surfaced;
* whether the helper preserves operation and cleanup errors;
* whether cancellation also performs cleanup.

A useful additional helper would be:

```ts
withDiscoveredConnection(manager, peerId, options, fn)
```

That would combine the overwhelmingly common connect → discover → use → release journey without hiding the underlying ownership model.

A public cleanup assertion/conversion helper would also reduce repeated generic code:

```ts
throwIfCleanupFailed(cleanup)
```

Ideally it should preserve the structured cleanup failures rather than replacing them with one string.

## `MIGRATION_4.0.md`

Keep the structure. Fix the executable issues and add:

* temporary coexistence with a strict one-radio-owner rule;
* feature-flagged migration and rollback;
* one full before/after lifecycle example;
* how to migrate adapter-state observation;
* how to migrate a persistent subscription during React component teardown;
* descriptor operations;
* restoration identity;
* background execution policy;
* error-code mapping;
* explicit explanation of `release()` versus `disconnect()`.

Also change “`requestConnectionPriority` → inspect capabilities.” That is not a replacement. Better:

> No direct 4.0 replacement. Inspect runtime capabilities and apply host-specific policy only where explicitly supported.

## `docs/PROFILES_AND_COMMANDS.md`

The duplicate-safe path explanation and codec validation material are strong. Keep those.

Fix:

* invalid profile reads;
* undefined `streamDelivery` in the example;
* optional/conditional Control Point handling;
* unbounded subscription before a later control-point write;
* examples using `signal: null` and `deadline: null`;
* property checks before invoking an operation.

The document says not to construct occurrences yourself but then uses literal occurrence strings. Clarify:

> Selector occurrences may be copied from a current snapshot. Full paths, database generations, and connection generations must never be constructed manually.

## `docs/EXPO_PLUGIN.md`

Add columns for:

* default;
* platform;
* common versus advanced;
* whether changing it requires prebuild/rebuild;
* operational effect;
* what it explicitly does **not** do.

Rename `isBackgroundEnabled`.

Give `neverForLocation` a stronger warning: Android describes it as a strong assertion and may filter some BLE beacons.

For restoration, add a lifecycle table:

| Field              | Stability                  | When it may change                          |
| ------------------ | -------------------------- | ------------------------------------------- |
| `identifier`       | App-install stable         | Intentional restoration namespace migration |
| `namespace`        | Journal-contract stable    | Explicit journal migration                  |
| `epoch`            | Versioned generation       | Intentional invalidation                    |
| `clientId`         | Logical BLE owner stable   | Ownership migration                         |
| `hostSessionScope` | Host security scope stable | Host boundary migration                     |

Do not use `signed-in-user-ble-client` as the primary example. A build-time Info.plist value cannot naturally follow arbitrary signed-in users. Use an app-owned identity such as `com.example.app.ble-client` unless the product has a concrete static mapping strategy.

## `docs/WEB.md`

The current setup asks ordinary consumers to wire:

* `navigator.bluetooth`;
* availability;
* browser engine identity;
* timers and timer-handle maps;
* secure-context checks;
* transient activation;
* page lifecycle listeners.

That is appropriate as an injectable environment for tests, but too much for normal browser setup.

Add a first-party default:

```ts
const session = await createNavigatorWebBleManager({
  clientId: 'web-app-ble-client',
  managerId: 'web-app-ble-manager'
})
```

Keep environment injection as an advanced/testing overload.

The guide should also include:

* chooser call directly inside a click handler;
* `getDevices()` for previously granted devices;
* Permissions Policy/iframe caveats;
* full connection cleanup;
* browser compatibility status;
* one finite read or notification example.

Web Bluetooth requires a secure context, `requestDevice()` requires transient activation, access can be controlled by the `bluetooth` Permissions Policy, and `getDevices()` can retrieve previously granted devices.

## `docs/NODE.md`

This is too short for the complexity of the actual Node host.

Add:

* one complete scan/connect/GATT/cleanup flow;
* adapter-selection rules when multiple adapters exist;
* macOS packaging and permission requirements;
* Windows prebuild/runtime requirements;
* Linux D-Bus service and authorization troubleshooting;
* ESM and CommonJS examples;
* native prebuild troubleshooting;
* expected Node versions;
* graceful process shutdown;
* error handling when the selected native backend is unavailable.

The current creation path also reveals API friction. Consumers must understand providers, compatibility descriptors, adapter listing, manager options, and ownership. Add first-party factories:

```ts
createCoreBluetoothBleManager(...)
createWinRtBleManager(...)
createBluezBleManager(...)
```

Keep `createBleManagerFromProvider` for advanced users and backend authors.

## `docs/ELECTRON.md`

The file is rigorous about security and ownership, but it is an architecture/security document rather than a runnable consumer guide. The PR description itself acknowledges that the renderer still lacks a copy-paste GATT loop.

Split it:

### `ELECTRON.md`

A complete runnable sequence:

1. create provider and main manager;
2. create router;
3. install binding;
4. authenticate `WebContents`;
5. expose a narrow preload bridge;
6. initialize renderer client;
7. scan;
8. connect;
9. discover;
10. read/subscribe;
11. release renderer;
12. destroy binding;
13. destroy manager.

### `ELECTRON_SECURITY_MODEL.md`

Move here:

* main-frame authentication;
* navigation cleanup;
* event acknowledgement;
* stream bounds;
* generation quarantine;
* deterministic evidence levels;
* VM membrane details;
* unsupported threat claims.

Also add actual BrowserWindow guidance:

* `contextIsolation: true`;
* `nodeIntegration: false`;
* sandboxing policy;
* no generic `ipcRenderer` exposure;
* CSP;
* ASAR unpacking;
* signing and notarization.

The current `example-electron` is a useful deterministic IPC/package proof, but it is not a real Electron application example. Build one before describing Electron as first-class developer-ready.

## `docs/TAURI.md`

The guide creates a manager, checks state, and starts a scan, then stops. It does not consume the scan, stop it, connect, discover, access characteristics, subscribe, release, or destroy in a complete journey.

Document the actual `IpcBleManager` surface:

* scan observations;
* overflow and terminal records;
* scan stop;
* connection;
* database characteristics/descriptors;
* characteristic read/write/subscribe;
* `timeoutMs`;
* connection release;
* manager destruction.

The implementation has fixed remote stream limits and a drop-oldest policy; users need to know how overflow is surfaced.

Also include:

* exact capability configuration;
* intended-window scoping;
* platform-specific native requirements;
* durable Cargo dependency guidance.

A Cargo path into `node_modules` is workable for a fixture but brittle as the principal consumer installation story. Publishing the Rust crate or generating a stable local path during installation would be a better long-term experience.

---

# Broader code and API improvements revealed by the documentation work

A good documentation rewrite often exposes where the API is making users perform framework plumbing. That happened here.

## Simplify React Native manager creation

The normal factory currently requires:

* platform;
* native control object;
* clock;
* client ID;
* manager ID;
* host-session scope.

That is excellent for explicit testing and security boundaries, but heavy for a normal application.

Provide two layers:

```ts
createReactNativeBleManager({
  clientId,
  managerId,
  hostSessionScope
})
```

and:

```ts
createReactNativeBleManagerWithEnvironment({
  platform,
  control,
  now,
  ...
})
```

The first infers supported RN platform, control module, and monotonic clock. The second remains injectable.

## Add scan presets

The explicit scan contract is valuable, but it takes many lines for the common case. Add safe presets:

```ts
defaultScanDelivery()
scanForServices([HEART_RATE_SERVICE], options)
scanForFirstMatch(...)
```

Keep the full bounded-stream options for applications that need exact control.

## Add adapter-state events

The migration guide currently tells users there is no public state watch and that they can poll `adapterState()` or observe app lifecycle. That is a meaningful ergonomics regression from `react-native-ble-plx`.

A cross-platform bounded stream such as:

```ts
manager.adapterStates
```

would fit the rest of the architecture and avoid application polling.

## Make cleanup a first-class public operation

Because every resource returns structured cleanup evidence, offer standard utilities for:

* retrying cleanup;
* preserving the primary operation error;
* aggregating multiple cleanup failures;
* formatting cleanup failures;
* observing resources that remain owned.

Otherwise every guide and every application will invent a weaker generic `if (state === 'release-failed') throw new Error(...)`.

## Decide what “unified” means at the consumer API

Today there are effectively three surfaces:

1. host-neutral `BleManager`;
2. desktop webview `IpcBleManager`;
3. Electron renderer client.

That can be intentional, but the product positioning and API design should agree.

The best outcome would be:

* identical high-level workflow names;
* host-specific construction and ownership;
* host-specific advanced capabilities;
* one shared data/error/stream model.

---

# Recommended PR sequence

## PR #26: documentation correctness

Keep this PR focused, but fix all canonical examples before merging:

1. Exception-safe finite README loop.
2. Optional profile handling.
3. Migration Battery write.
4. Migration cancellation and coexistence guidance.
5. Platform guard.
6. Shared-deadline explanation.
7. Correct “shared semantics” positioning.
8. Complete License section.
9. Expo install/prebuild corrections.
10. Current package-version wording.

## Immediate pre-stable API correction PR

Do this before publishing stable 4.0:

1. Rename `isBackgroundEnabled`.
2. Resolve `peripheral` mode exposure.
3. Add new debug environment variable.
4. Remove or correct invalid standard-profile read commands.
5. Validate characteristic properties in profile commands.
6. Stabilize identity examples.
7. Fix example-service lifecycle races and cleanup aggregation.

Because there are no production users, delaying these changes would only create avoidable deprecations.

## Documentation verification PR

1. Compile Markdown TypeScript fences.
2. Execute finite snippets using deterministic backend.
3. Check links and anchors.
4. Add semantic BLE rules.
5. Generate version and option references.
6. Replace exact source-parity tests with behavioral tests.

## Host experience PRs

Split these rather than making one massive documentation patch:

* Web zero-config factory and guide.
* Electron runnable application and guide.
* Tauri complete application flow.
* Node one-call factories and troubleshooting.
* React Native/Expo/bare setup separation.

---

# Recommended README hierarchy

The final README should be closer to this:

1. **Unified BLE Manager**
2. One-sentence value proposition
3. API/backend maturity callout
4. Sponsor note
5. Choose your host
6. Install
7. Five-minute finite example
8. Core mental model
9. Public entrypoints
10. Method index
11. Migration
12. Examples
13. Support and security
14. Development
15. License

Keep the deep architecture and evidence model linked, but do not make normal users internalize proof levels before connecting their first peripheral.

---

# Final merge recommendation

**Do not close or rewrite this PR from scratch. The direction is good and most of the prose should survive.**

My recommendation is:

* **Request changes now.**
* Fix the canonical cleanup, profile assumptions, migration errors, cancellation examples, and version/license inconsistencies.
* Correct the profile-command and Expo-option APIs before stable 4.0 rather than documenting misleading names and operations.
* Merge PR #26 once the examples are semantically safe.
* Treat runnable Electron/Tauri/Node documentation and executable Markdown testing as the second documentation round.

The first round succeeded at making the project understandable. The next pass needs to make every prominent example trustworthy.

## External references cited by the reviewer

- [Heart Rate Service v1.0](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/HRS_v1.0/out/en/index-en.html)
- [Blood Pressure Service v1.1.1](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/BLS_v1.1.1/out/en/index-en.html)
- [Android Bluetooth permissions](https://developer.android.com/develop/connectivity/bluetooth/bt-permissions)
- [Expo using libraries](https://docs.expo.dev/workflow/using-libraries/)
- [Installing Expo modules in bare RN](https://docs.expo.dev/bare/installing-expo-modules/)
- [MDN Web Bluetooth API](https://developer.mozilla.org/en-US/docs/Web/API/Web_Bluetooth_API)

---

# Extracted claims to verify

These claims are the reviewer’s factual assertions about this checkout. They are the verification checklist.

| ID | Domain | Claim |
| --- | --- | --- |
| C1 | README | The complete-loop example creates connection/database/subscription and only cleans up via trailing statements; thrown errors skip remaining cleanup. |
| C2 | README | The notification `for await` loop has no stopping condition; `AbortController` is created but never externally aborted. |
| C3 | README | The loop unconditionally resolves Battery Level, Heart Rate Measurement, and Heart Rate Control Point after scanning for Heart Rate Service. |
| C4 | README | Opening copy claims “Same manager contract on every host.” |
| C5 | README | `## License` is empty; package license is Apache-2.0. |
| C6 | README | Sponsor line appears before the H1 / product statement. |
| C7 | README | README claims Bun works; `package.json` engines are Node versions. |
| C8 | README | README uses `duplicatePolicy: 'merged'` for name-dependent matching. |
| C9 | README | Wording includes “bound live-radio receipt”. |
| C10 | README | Package version is hard-coded on multiple pages. |
| C11 | MIGRATION | A sample reads then writes Battery Level (`batteryLevelSelector()` + write `[1]`). |
| C12 | MIGRATION | `abort.abort()` appears after an unbounded `for await` loop. |
| C13 | MIGRATION | Early text says do not keep both packages; later “Suggested order” says install alongside then remove. |
| C14 | MIGRATION | Name-dependent scan uses `duplicatePolicy: 'first'`. |
| C15 | MIGRATION | One `until` / deadline is reused across scan, connect, discover, read, write, subscribe without explaining it is a shared budget. |
| C16 | MIGRATION | `requestConnectionPriority` is presented as replaced by “inspect capabilities.” |
| C17 | API | `src/profiles/standard-commands.ts` exports `readHeartRateMeasurement`, `readTemperatureMeasurement`, `readBloodPressureMeasurement`. |
| C18 | API | Those helpers call `database.read`/`write`/`subscribe` without checking discovered characteristic properties. |
| C19 | API | Heart Rate Measurement is Notify-only in the SIG; Blood Pressure Measurement is Indicate-only. |
| C20 | Example | Example service `operation()` always returns `{ signal: null, deadline: null }`. |
| C21 | Example | Example README claims the fixture demonstrates `AbortSignal`. |
| C22 | Example | Scan and notification consumers handle `value` and `terminal` but not `overflow`. |
| C23 | Example | `findCharacteristicPath` uses `.find()` on UUID pair and silently collapses duplicates. |
| C24 | Example | `readProfileValue` catches every error and returns `{ skipped: true }`. |
| C25 | Example | `destroy()` can return while `ensureManager()` is still pending, then assign a live manager. |
| C26 | Example | `destroy()` runs `stopScan` → `disconnect` → `manager.destroy` sequentially and short-circuits on throw. |
| C27 | Example | `clientId` and `managerId` include an incrementing number. |
| C28 | Example | Example service tries to read Temperature Measurement and Blood Pressure Measurement. |
| C29 | Plugin | `isBackgroundEnabled` only adds Android BLE hardware feature; does not enable background execution. |
| C30 | Plugin | Plugin accepts iOS background mode `peripheral`. |
| C31 | Plugin | Debug env var is `BLEPLX_PLUGIN_DEBUG`; no `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG`. |
| C32 | Plugin | `neverForLocation` is documented without a strong Android warning. |
| C33 | Plugin | Restoration example uses `signed-in-user-ble-client`. |
| C34 | GETTING_STARTED | Promises a first journey then sends readers back to the README for the loop. |
| C35 | GETTING_STARTED / docs | `Platform.OS === 'ios' ? 'apple' : 'android'` appears in docs; example service already guards. |
| C36 | HELPERS | Helpers `scanUntil`, `connectAndDiscover`, `firstNotification`, `collectNotifications`, `withConnection` exist and preserve operation + cleanup errors. |
| C37 | HOSTS | Tauri docs/API return `IpcBleManager`; Electron renderer uses a client, not radio ownership. |
| C38 | HOSTS | WEB.md requires consumers to wire navigator/availability/timers/activation/lifecycle. |
| C39 | HOSTS | NODE.md lacks a complete GATT journey and one-call factories. |
| C40 | HOSTS | ELECTRON.md is security/architecture-heavy and lacks a copy-paste GATT loop. |
| C41 | HOSTS | TAURI.md starts a scan then stops without connect/discover/GATT/cleanup. |
| C42 | TESTS | `Docs.consumer.test.js` is mostly `toContain` / regex / filename assertions. |
| C43 | TESTS | Example parity test asserts shared source patterns including profile reads. |
| C44 | PROFILES | `docs/PROFILES_AND_COMMANDS.md` has undefined `streamDelivery`, `signal: null`/`deadline: null`, and/or literal occurrence strings after warning not to construct occurrences. |
| C45 | RN factory | `createReactNativeBleManager` requires platform, control, clock, clientId, managerId, hostSessionScope. |
| C46 | Adapter | There is no public adapter-state stream; migration tells users to poll `adapterState()`. |
| C47 | Surfaces | Public consumer surfaces include `BleManager`, `IpcBleManager`, and Electron renderer client. |
