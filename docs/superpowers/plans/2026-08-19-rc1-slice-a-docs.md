# Slice A — documentation correctness (plus D-docs)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Execute **after Slice B and Slice D-code**. Pair with D-docs tasks D7–D11 in `2026-08-19-rc1-slice-d-hosts.md`. Read the master plan first.

**Goal:** Make every prominent consumer example finite, exception-safe, and SIG-honest against the **post-B/D** public API.

**Architecture:** One documentation pass. Helpers own scan/connection/subscription cleanup. First journey is Heart Rate Measurement notify only. Host pages use convenience factories.

**Tech Stack:** Markdown consumer docs, `__tests__/Docs.consumer.test.js` string guards (Slice C adds semantic tests after this pass).

---

## Files

| File | Job |
| --- | --- |
| `README.md` | Hierarchy, finite helper journey, wording |
| `docs/GETTING_STARTED.md` | Self-contained journey + three RN install paths |
| `docs/TUTORIALS.md` | Finite recipes |
| `docs/HELPERS.md` | Ownership/deadline/cleanup notes; new helpers |
| `MIGRATION_4.0.md` | Executable mistakes + coexistence + adapterStates |
| `docs/PROFILES_AND_COMMANDS.md` | No invalid reads; defined delivery; occurrence rule |
| `docs/EXPO_PLUGIN.md` | Finish option table if B left gaps |
| `docs/WEB.md` `NODE.md` `ELECTRON.md` `TAURI.md` | D-docs (see slice D) |
| `__tests__/Docs.consumer.test.js` | Update wording assertions to the new text |

Do not edit Tauri freeze paths. Do not reintroduce `isBackgroundEnabled`, invalid SIG reads, or the ios/android ternary.

---

### Task A1: Update Docs.consumer tests first (red)

Modify `__tests__/Docs.consumer.test.js` so current README **fails** until the new journey exists.

Assert README contains:

- `withConnection` or `withDiscoveredConnection`
- `firstNotification` or `collectNotifications`
- `createReactNativeBleManager({`
- `journeyDeadline` or an explicit “shared budget” sentence
- `SECURITY.md` and `SUPPORT.md`
- `Apache License 2.0` (already true)

Assert README does **not** contain:

- `Same manager contract on every host`
- `bound live-radio receipt`
- `isBackgroundEnabled`
- `readHeartRateMeasurement`
- `Platform.OS === 'ios' ? 'apple' : 'android'`
- `batteryLevelSelector` inside the first complete-loop fence (Battery belongs in tutorials)

Assert `MIGRATION_4.0.md` does **not** contain `duplicatePolicy: 'first'` next to a `localName` match.

Run:

```sh
pnpm exec jest __tests__/Docs.consumer.test.js --runInBand
```

Expected: FAIL on this tree.

---

### Task A2: README hierarchy and first journey

Order:

1. `# Unified BLE Manager`
2. One-sentence product statement (central library, host-picked radio, bytes, AbortSignal, destroy)
3. Maturity callout: package/API is `4.0.0-rc.0` until the later version bump; backends stay Experimental until artifact-bound physical-hardware validation; link `docs/PLATFORMS.md`
4. Sponsor line (currently first; move it here)
5. Host chooser table (existing entrypoints)
6. Install (`installable with npm, yarn, or Bun` — package managers, not a Bun runtime claim)
7. Five-minute finite example (below)
8. Why the API looks like this
9. Public entrypoints
10. Method index (include `adapterStates`, convenience factories, helpers)
11. Migration / examples links
12. Support + `SECURITY.md` + `SUPPORT.md`
13. Development
14. License (leave Apache-2.0 text)

Canonical first journey — copy this shape (adjust only if a helper name from D-code differs):

```ts
import {
  deadline,
  defaultScanDelivery,
  firstNotification,
  scanUntil,
  throwIfCleanupFailed,
  withConnection
} from 'unified-ble-manager'
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import {
  HEART_RATE_SERVICE,
  heartRateMeasurementSelector,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'

const manager = await createReactNativeBleManager({
  clientId: 'com.example.app.ble-client',
  managerId: 'com.example.app.ble-manager',
  hostSessionScope: 'com.example.app'
})

const abort = new AbortController()
const journeyDeadline = deadline(manager.monotonicNow() + 20_000)
const op = { signal: abort.signal, deadline: journeyDeadline }

try {
  const observation = await scanUntil(manager, {
    scan: {
      filter: {
        serviceUuids: [HEART_RATE_SERVICE],
        manufacturerData: [],
        localNamePrefix: null
      },
      duplicatePolicy: 'merged',
      timestampPolicy: 'source-then-receipt',
      delivery: defaultScanDelivery(),
      deadline: journeyDeadline,
      signal: abort.signal,
      sharing: { mode: 'owner', allowSharing: false }
    },
    matches: candidate => candidate.localName.state === 'present'
  })

  await withConnection(manager, observation.device.id, op, async connection => {
    const database = await connection.discover(op)
    const snapshot = await database.snapshot()
    const measurementPath = await resolveCharacteristicPath(snapshot, heartRateMeasurementSelector())
    const bytes = await firstNotification(database, measurementPath, {
      ...op,
      delivery: defaultScanDelivery()
    })
    consume(parseHeartRateMeasurement(bytes))
  })
} finally {
  throwIfCleanupFailed(await manager.destroy(), 'manager.destroy')
}
```

Notes the README must state:

- `journeyDeadline` is one budget for the whole sample, not 20s per call.
- Battery Level and Heart Rate Control Point are optional/conditional; link tutorials.
- Persistent subscriptions live in `docs/TUTORIALS.md`.
- Web uses `createNavigatorWebBleManager` + chooser from a click; Tauri/Electron renderer are different client types.

Add a compact host-requirements table (RN 0.86+ / Expo 57+ / secure context for Web / Electron main owns radio) **before** RN-only material.

---

### Task A3: GETTING_STARTED

Must include the same finite journey (or the exact fence, not “copy the README”).

Three install paths:

1. **Expo/CNG:** `npx expo install unified-ble-manager`; plugin `requiresBluetoothLeHardware`; `npx expo prebuild`; dev client or production binary; not Expo Go; config changes need a native rebuild.
2. **Bare RN + Expo modules:** `npx install-expo-modules@latest` first, then the plugin.
3. **Bare RN without Prebuild:** Android manifest + runtime permissions, iOS usage string, pods, rebuild. No silent “everything else is Android.”

Platform helper (docs and snippets):

```ts
function reactNativeBlePlatform(os: string): 'android' | 'apple' {
  if (os === 'android') return 'android'
  if (os === 'ios') return 'apple'
  throw new Error(`Unsupported React Native platform: ${os}`)
}
```

App factory does this internally after D-code; GETTING_STARTED should not pass `platform` unless showing `WithEnvironment`.

---

### Task A4: TUTORIALS and HELPERS

`docs/TUTORIALS.md` — independently runnable, each recipe cleans up on failure:

1. Find one peripheral (`scanUntil` / `scanForServices`)
2. Connect + discover (`withDiscoveredConnection`)
3. Read one characteristic that is actually readable (Battery Level **after** snapshot shows it)
4. Write one characteristic that is actually writable (app UUID placeholders, or Heart Rate Control Point **after** `gatt.not-found` / property check)
5. One notification (`firstNotification`)
6. Long-lived subscription (abort from another task / UI; `abort.abort()` **before** the loop ends)
7. Overflow (`item.kind === 'overflow'`)
8. Disconnect / reconnect (app-owned; link CONNECTION_MANAGER)
9. Tear down (`throwIfCleanupFailed(await manager.destroy(), ...)`)

Optional recipes: “Read battery when present”, “Reset energy expended when supported” — catch only `gatt.not-found` and `gatt.property-not-supported`.

`docs/HELPERS.md` — for each public helper (`scanUntil`, `find`, `scanForServices`, `connectAndDiscover`, `withConnection`, `withDiscoveredConnection`, `firstNotification`, `collectNotifications`, `throwIfCleanupFailed`):

- who owns the connection after success
- whether the deadline is shared
- how cleanup failures surface
- whether operation + cleanup errors are preserved (`AggregateError`)
- whether cancellation also cleans up

Promote HELPERS from the README map (already linked).

---

### Task A5: MIGRATION_4.0.md

Keep the side-by-side structure. Fix:

- Readable path and writable path are **different** variables. Never write Battery Level.
- Cancellation: `AbortController` aborted from a timer, UI, or `firstNotification` — never `abort.abort()` after `for await`.
- Coexistence: “Both packages may be installed temporarily. Only one BLE stack may own the radio/session.” Remove the contradiction with “do not keep both packages.”
- Name-dependent scan: `duplicatePolicy: 'merged'`.
- Shared deadline named `journeyDeadline` with one sentence of explanation.
- `requestConnectionPriority`: “No direct 4.0 replacement. Inspect `manager.capabilities()` and apply host-specific policy only where explicitly supported.”
- `state()` / `onStateChange` → `adapterState()` snapshot **and** `adapterStates()` stream.
- Construction: `createReactNativeBleManager({ clientId, managerId, hostSessionScope })`.
- Keep “Gone on purpose.”
- Add short notes: `release()` vs `disconnect()`; feature-flagged rollback; do not run two managers on one adapter.

---

### Task A6: PROFILES_AND_COMMANDS.md

- Delete taught `readHeartRateMeasurement` / `readBloodPressureMeasurement` / `readTemperatureMeasurement`.
- Use `subscribeHeartRateMeasurements` / `firstNotification`.
- Define `delivery: defaultScanDelivery()` (or a local `const delivery = defaultScanDelivery()`).
- Do not use `{ signal: null, deadline: null }` as the taught default; use a real controller + `journeyDeadline`.
- Clarify: selector occurrences may be copied from a current snapshot. Full paths, database generations, and connection generations must not be invented.

---

### Task A7: D-docs

Execute tasks D7–D11 in `2026-08-19-rc1-slice-d-hosts.md` in the same pass.

---

### Task A8: Green the consumer-doc tests

```sh
pnpm exec jest __tests__/Docs.consumer.test.js --runInBand
```

Expected: PASS.

Grep the consumer set (`README.md`, `docs/*.md`, `MIGRATION_4.0.md`) for leftovers:

```sh
rg -n "Same manager contract|bound live-radio receipt|isBackgroundEnabled|readHeartRateMeasurement|readBloodPressureMeasurement|readTemperatureMeasurement|Platform\\.OS === 'ios' \\? 'apple'" README.md MIGRATION_4.0.md docs/GETTING_STARTED.md docs/TUTORIALS.md docs/HELPERS.md docs/WEB.md docs/NODE.md docs/ELECTRON.md docs/TAURI.md docs/PROFILES_AND_COMMANDS.md docs/EXPO_PLUGIN.md
```

Expected: no matches in those teaching pages. Maintainer/archive docs may keep historical names.
