# Verification of the 2026-08-19 PR #26 external review

Verified: 2026-08-19  
Checkout: `docs/markdown-first-4x-rewrite` at `0f48e775093337a69dd1c3ce4a05d9bcc828ff41`  
Package: `4.0.0-rc.0`  
Source review: [`2026-08-19-pr-26-external-review.md`](./2026-08-19-pr-26-external-review.md)

Six explore agents checked the tree. SIG characteristic properties were checked independently against the adopted Heart Rate, Blood Pressure, Health Thermometer, and Battery service definitions.

## Result

The review is **substantially correct** and should drive the `4.0.0-rc.1` work. Of 47 extracted claims:

| Status | Count | Meaning |
| --- | ---: | --- |
| confirmed | 41 | Reviewer was factually right |
| partial | 5 | Directionally right; wording overstated or missing nuance |
| refuted | 1 | License section is already filled |
| not-found | 0 | |

No claim was invented. One claim is stale (C5). One claim is stronger than the reviewer stated: Health Thermometer Temperature Measurement is also **Indicate-only**, so `readTemperatureMeasurement` should be removed with the other invalid standard reads.

The reviewer’s merge recommendation stands: **do not merge PR #26 unchanged**. The direction is good. Canonical examples, migration snippets, profile commands, and the example service are not yet release-quality.

## Verdict on the reviewer’s verdict

| Reviewer judgment | Verification |
| --- | --- |
| Strong first documentation round / right hierarchy | Confirmed as a qualitative assessment of the branch vs `main`. |
| Do not approve PR #26 unchanged | Confirmed. Multiple copy-paste examples leak resources or fail on valid SIG peripherals. |
| Helpers are the best teaching surface | Confirmed. `scanUntil`, `connectAndDiscover`, `firstNotification`, `collectNotifications`, and `withConnection` exist and preserve operation + cleanup errors via `settleWithCleanup` / `AggregateError`. |
| Profile-command issue is a public-API defect, not just docs | Confirmed, and HTS is in the same class as HRS/BLS. |
| Empty License section | **Refuted** on this head. `README.md` already says Apache License 2.0. |

---

## Claim table

| ID | Domain | Status | Notes |
| --- | --- | --- | --- |
| C1 | README | **partial** | The complete loop already uses `scanUntil` for scan. Subscription cleanup is in `try/finally`. `connection.release()` and `manager.destroy()` are still trailing. A throw before the `try`, or from `subscription.remove()`, skips remaining cleanup. The leak/lifecycle point is real. |
| C2 | README | **partial** | The notify loop breaks on a terminal record and already reports overflow. `AbortController` is created and never `.abort()`’d. There is a 20s `until`, but the `for await` still has no external stop. The “subscribe forever then teardown after the loop” problem remains. |
| C3 | README | **confirmed** | After an HRS-only scan, the loop unconditionally resolves Battery Level, Heart Rate Measurement, and Heart Rate Control Point. |
| C4 | README | **confirmed** | “Same manager contract on every host.” Later text correctly admits Tauri/Electron renderer use different client types. |
| C5 | README | **refuted** | `## License` is filled: “Apache License 2.0. See `LICENSE` and `THIRD_PARTY_LICENSES.json`.” Package license is `Apache-2.0`. |
| C6 | README | **confirmed** | Sponsor blockquote is line 3; H1 is line 5. |
| C7 | README | **partial** | Copy says “Works with npm, yarn, or Bun” in the **install** section (package manager). Engines are Node only. Reviewer read this as a Bun runtime claim. Still worth tightening to “installable with Bun.” |
| C8 | README | **confirmed** | Name-dependent match uses `duplicatePolicy: 'merged'`. |
| C9 | README | **confirmed** | “until a bound live-radio receipt says otherwise.” |
| C10 | README | **confirmed** | `4.0.0-rc.0` is hand-copied across README, GETTING_STARTED, WEB, NODE, ELECTRON, MIGRATION, example README, generated PLATFORM_SUPPORT. |
| C11 | MIGRATION | **confirmed** | Same Battery Level path is read, then written `[1]`. Battery Level Write is SIG-excluded. |
| C12 | MIGRATION | **confirmed** | `abort.abort()` sits after `for await (const item of sub.values)`. |
| C13 | MIGRATION | **confirmed** | Early: “Do not keep both packages.” Later suggested order: install next to the old package, then remove. |
| C14 | MIGRATION | **confirmed** | Name match + `duplicatePolicy: 'first'`. |
| C15 | MIGRATION | **confirmed** | One `until = deadline(now + 20_000)` is reused for scan/connect/discover/read/write/subscribe with no shared-budget explanation. |
| C16 | MIGRATION | **confirmed** | `` `requestConnectionPriority` \| Inspect `manager.capabilities()`. `` |
| C17 | API | **confirmed** | `readHeartRateMeasurement`, `readTemperatureMeasurement`, `readBloodPressureMeasurement` are exported from `src/profiles/standard-commands.ts` plus matching subscribe helpers. |
| C18 | API | **confirmed** | Helpers only `resolveCharacteristicPath` then `database.read`/`write`/`subscribe`. Core `database.read` does not fail closed on missing Read. Some backends (Web) do. |
| C19 | SIG | **confirmed** | HRS Heart Rate Measurement: Read excluded, Notify mandatory. BLS Blood Pressure Measurement: Read excluded, Indicate mandatory. **HTS Temperature Measurement is also Read excluded, Indicate mandatory.** Battery Level: Read mandatory, Write excluded. HRS has no Battery Service dependency. |
| C20 | Example | **confirmed** | `operation()` always returns `{ signal: null, deadline: null }`. |
| C21 | Example | **confirmed** | `example/README.md` claims the fixture demonstrates `AbortSignal`. Expo README does not. |
| C22 | Example | **confirmed** | Scan/notification consumers handle `value` and `terminal` only. Overflow is dropped. Both still set `overflowPolicy: 'drop-oldest'`. |
| C23 | Example | **confirmed** | `findCharacteristicPath` uses `.find()` on UUID pair. |
| C24 | Example | **confirmed** | `readProfileValue` catches every error and returns `{ skipped: true }`. |
| C25 | Example | **confirmed** | `destroy()` returns when `this.manager === null` without awaiting `managerCreation`. Late `this.manager = manager` can land after destroy. |
| C26 | Example | **confirmed** | Sequential `stopScan` → `disconnect` → `manager.destroy` with no aggregation. `disconnect()` starts with `stopNotification()`. |
| C27 | Example | **confirmed** | `clientId` and `managerId` include incrementing `nextExampleManagerId`. |
| C28 | Example | **confirmed** | Both bare and Expo services `database.read` Temperature Measurement and Blood Pressure Measurement. |
| C29 | Plugin | **confirmed** | `isBackgroundEnabled` only writes `android.hardware.bluetooth_le` `required="true"`. |
| C30 | Plugin | **confirmed** | iOS background mode `peripheral` is accepted by schema, validator, tests, and docs. |
| C31 | Plugin | **confirmed** | Only `BLEPLX_PLUGIN_DEBUG`. No `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG`. |
| C32 | Plugin | **partial** | Docs already say set `neverForLocation` only when the product makes that assertion. They do not mention Android’s beacon-filter warning. |
| C33 | Plugin | **confirmed** | Docs/README use `signed-in-user-ble-client`. Plugin tests already use `com.example.app.ble.client`. |
| C34 | GETTING_STARTED | **confirmed** | Promises a first journey, then “Copy the complete loop from the root README.” |
| C35 | Docs | **confirmed** | `Platform.OS === 'ios' ? 'apple' : 'android'` in README, GETTING_STARTED, and MIGRATION. Example service already throws on anything else. |
| C36 | Helpers | **confirmed** | All five helpers exist. `settleWithCleanup` preserves operation + cleanup errors (`AggregateError` when both fail). |
| C37 | Hosts | **confirmed** | Tauri returns `IpcBleManager`. Electron renderer is `ElectronRendererBleClient` and cannot select a radio. |
| C38 | WEB | **confirmed** | The only factory sample injects navigator, timers, activation, lifecycle. |
| C39 | NODE | **confirmed** | No complete GATT journey. No `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`. |
| C40 | ELECTRON | **confirmed** | Security/ownership document. No copy-paste GATT loop. |
| C41 | TAURI | **confirmed** | Sample is create → `adapterState` → `scan({ serviceUuids })`. No connect/discover/GATT. Scan is never stopped in the snippet. |
| C42 | Tests | **confirmed** | `Docs.consumer.test.js` is string/regex/filename assertions. |
| C43 | Tests | **confirmed** | Parity test asserts shared source, including the invalid profile reads. |
| C44 | PROFILES | **confirmed** | Undefined `streamDelivery`, `signal: null`/`deadline: null`, literal `{ serviceOccurrence: '0' }` after “do not construct occurrences.” |
| C45 | RN factory | **confirmed** | Required: `platform`, `control`, `now`, `clientId`, `managerId`, `hostSessionScope`. No zero-config factory. |
| C46 | Adapter | **confirmed** | Public `adapterState()` is a snapshot. Migration tells users to poll. Backend-private watchers exist. |
| C47 | Surfaces | **confirmed** | Three consumer façades: `BleManager`, `IpcBleManager`, `ElectronRendererBleClient`. |

---

## Extra findings (not in the original claim list)

1. **`readTemperatureMeasurement` is invalid for the same reason as the HRS/BLS reads.** HTS Temperature Measurement: Read excluded, Indicate mandatory. The reviewer’s “verify HTS before retaining the read helper” is resolved: remove it.
2. **`SECURITY.md` and `SUPPORT.md` are not linked from the README** (reviewer requested this; not a factual claim about existing text, but confirmed as a gap).
3. **README already uses `scanUntil` for the first scan.** The remaining problem is the raw connect/discover/optional-profile/unbounded-subscribe tail, not a fully primitive first journey.
4. **Core `database.read` does not enforce characteristic properties.** Web fails closed. Deterministic read does not. A standard-command property check would make the contract host-independent.

---

## What this does *not* decide

- Whether Tauri and Electron renderer should converge on one high-level façade (product decision).
- Whether `adapterStates` ships in `4.0.0-rc.1` or later (ergonomics vs contract growth).
- Whether host-experience work (Web factory, Electron app, Tauri full flow, Node one-call factories) is in rc.1 or a later RC.

Those are sequencing decisions for the implementation plan.

---

## Recommended response posture

Treat the review as accepted except C5.

Must-fix before this docs PR is mergeable:

- Finite, exception-safe first journey.
- Optional/conditional profile handling.
- Migration Battery write, abort-after-loop, coexistence rule, `merged` scan policy, shared-deadline wording, `requestConnectionPriority` wording.
- Platform guard in docs.
- “Shared model and semantics” instead of “same manager contract.”
- Sponsor/H1 order, Bun wording, live-radio wording, SECURITY/SUPPORT links.

Must-fix before stable 4.0, and should ship in `4.0.0-rc.1` because there are no production users:

- Remove invalid standard-profile reads (HRS, BLS, **and HTS**).
- Property validation in profile commands.
- Rename `isBackgroundEnabled`.
- `peripheral` policy and `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG`.
- Example-service lifecycle, overflow, identity, optional-error handling.

Second wave (can be later RCs if rc.1 needs to stay shippable):

- Executable Markdown / semantic doc tests.
- Web zero-config factory.
- Electron runnable app + security-doc split.
- Tauri complete journey + Cargo story.
- Node one-call factories.
- RN zero-config factory, scan presets, adapter-state stream, public cleanup helpers.
