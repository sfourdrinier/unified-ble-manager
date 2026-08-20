# Slice B — pre-stable API and example corrections

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Execute after reading `docs/superpowers/plans/2026-08-19-rc1-review-response.md`. This is **step 1** of rc.1.

**Goal:** Make standard-profile commands SIG-correct, rename the Expo option that does not enable background, and make the RN example match the contract the docs will teach.

**Architecture:** Remove invalid SIG read helpers; validate characteristic properties in `src/profiles/commands.ts` before calling the database; plugin option rename with no alias; example service owns cleanup and identity correctly.

**Tech Stack:** Jest, `unified-ble-manager/profiles/standard-commands`, Expo config plugin, bare + Expo example services.

---

## Files

| File | Responsibility |
| --- | --- |
| `src/profiles/commands.ts` | Property gate before read/write/subscribe |
| `src/profiles/standard-commands.ts` | Delete invalid reads; keep subscribe + legal reads/writes |
| `__tests__/profiles/` (create if missing) or extend PackageSurface tests | Export and property-gate coverage |
| `plugin/src/withBLE.ts` | Option rename; reject `peripheral` |
| `plugin/src/withBLEAndroidManifest.ts` | Read `requiresBluetoothLeHardware` |
| `plugin/src/withBLEBackgroundModes.ts` | Central only |
| `plugin/src/debugLog.ts` | `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG` + old alias |
| `plugin/src/__tests__/withBLE-test.ts` | Plugin tests |
| `docs/EXPO_PLUGIN.md` | Option table (minimal B update so plugin tests/docs do not lie) |
| `example-expo/app.json` | New option name |
| `example/src/services/BLEService/BLEService.ts` | Canonical service |
| `example-expo/src/services/BLEService/BLEService.ts` | Same behavioral contract |
| `example/README.md` | AbortSignal claim matches code |
| `__tests__/ExampleBleService.parity.test.js` | Behavioral parity, not “same invalid read twice” |

Do not edit Tauri freeze paths. Do not rewrite README beyond strings that would fail existing tests (`isBackgroundEnabled` mentions).

---

### Task B1: Failing tests for invalid standard reads and property gates

**Files:**
- Create: `__tests__/profiles/standard-commands.test.js`
- Modify: `__tests__/PackageSurface4.test.js` only if it currently requires the removed exports
- Modify: `__tests__/package-surface/fixtures/public-surface.ts` if it imports a removed name

- [ ] **Step 1: Write the failing test file**

```js
const { BackendContractError } = require('unified-ble-manager')
const standard = require('unified-ble-manager/profiles/standard-commands')
const { readCharacteristic, writeCharacteristic, subscribeCharacteristic, characteristicSelector } = require('unified-ble-manager/profiles/commands')

describe('standard-commands SIG surface', () => {
  test('does not export invalid SIG reads', () => {
    expect(standard.readHeartRateMeasurement).toBeUndefined()
    expect(standard.readBloodPressureMeasurement).toBeUndefined()
    expect(standard.readTemperatureMeasurement).toBeUndefined()
  })

  test('keeps legal helpers', () => {
    expect(typeof standard.subscribeHeartRateMeasurements).toBe('function')
    expect(typeof standard.subscribeBloodPressureMeasurements).toBe('function')
    expect(typeof standard.subscribeTemperatureMeasurements).toBe('function')
    expect(typeof standard.resetHeartRateEnergyExpended).toBe('function')
    expect(typeof standard.readBatteryLevel).toBe('function')
    expect(typeof standard.readBodySensorLocation).toBe('function')
  })
})
```

Add a property-gate test that uses `unified-ble-manager/testing` to build a discovered database whose Heart Rate Measurement has `notify: true, read: false`, then:

```js
await expect(readCharacteristic(database, heartRateMeasurementSelector(), op)).rejects.toMatchObject({
  code: 'gatt.property-not-supported'
})
```

Mirror for write on Battery Level (`write: false`) and subscribe on a read-only Body Sensor Location (`notify: false`).

Use the existing deterministic backend helpers in `__tests__/manager` / `src/testing` — copy the smallest working fixture from `src/tck/deterministic` or `__tests__/scenarios`. Do not invent a second mock backend.

- [ ] **Step 2: Run the test and confirm it fails**

```sh
pnpm exec jest __tests__/profiles/standard-commands.test.js --runInBand
```

Expected: FAIL because the three read helpers still exist and `readCharacteristic` does not inspect properties.

- [ ] **Step 3: Update surface fixtures if they import removed names**

`__tests__/package-surface/fixtures/public-surface.ts` currently imports `readBatteryLevel, subscribeHeartRateMeasurements` — keep those. If any fixture imports `readHeartRateMeasurement`, switch it to subscribe.

---

### Task B2: Property validation then remove invalid reads

**Files:**
- Modify: `src/profiles/commands.ts`
- Modify: `src/profiles/standard-commands.ts`

- [ ] **Step 1: Gate operations in `commands.ts`**

After `resolveCharacteristicPath`, load the matching snapshot characteristic and:

- `readCharacteristic`: require `properties.read === true`
- `writeCharacteristic`: require `writeWithResponse` when `mode === 'with-response'`, `writeWithoutResponse` when `mode === 'without-response'`
- `subscribeCharacteristic`: require `properties.notify === true` **or** `properties.indicate === true` if the snapshot exposes indicate. Today the snapshot type in `src/backend-contract/gatt.ts` may only list `notify`. If indicate is collapsed into `notify` (Tauri IPC currently does this), treat `notify === true` as sufficient for subscribe. Do not add a new snapshot field in this slice unless the type already has it.

Throw `contractError('gatt.property-not-supported', 'gatt', 'profiles.read-characteristic')` (and write/subscribe operation names) **before** `database.read` / `write` / `subscribe`.

- [ ] **Step 2: Delete these functions from `src/profiles/standard-commands.ts`**

- `readHeartRateMeasurement`
- `readBloodPressureMeasurement`
- `readTemperatureMeasurement`

Keep parse helpers and subscribe helpers. Keep `readBatteryLevel` and `readBodySensorLocation`.

- [ ] **Step 3: Re-run tests**

```sh
pnpm exec jest __tests__/profiles/standard-commands.test.js __tests__/PackageSurface4.test.js --runInBand
```

Expected: PASS.

---

### Task B3: Expo plugin rename, debug env, reject peripheral

**Files:** plugin sources and tests listed above; `example-expo/app.json`; `docs/EXPO_PLUGIN.md`; any README plugin JSON that still says `isBackgroundEnabled`.

- [ ] **Step 1: Failing plugin tests**

In `plugin/src/__tests__/withBLE-test.ts`:

- Configuring `isBackgroundEnabled` throws (unknown property) **or** is simply not in the schema — the option name must not work.
- `requiresBluetoothLeHardware: true` adds `android.hardware.bluetooth_le` `android:required="true"`.
- `modes: ['peripheral']` throws: unsupported, central-only library.
- `modes: ['central']` still writes `bluetooth-central`.
- Debug is enabled when `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG=1`.
- Debug is still enabled when `BLEPLX_PLUGIN_DEBUG=1` (alias).
- Restoration example identity in docs tests, if any, uses `com.example.app.ble-client`.

- [ ] **Step 2: Implement**

`UnifiedBlePluginOptions`:

```ts
readonly requiresBluetoothLeHardware?: boolean
```

Remove `isBackgroundEnabled` from `pluginOptionNames`.

`withBLEBackgroundModes.ts`: delete `BackgroundMode.Peripheral` and the `bluetooth-peripheral` branch. Validator in `withBLE.ts` only accepts `'central'`.

`debugLog.ts`:

```ts
const env =
  process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG ?? process.env.BLEPLX_PLUGIN_DEBUG
```

Log prefix: `[UNIFIED_BLE_MANAGER_PLUGIN]`.

`withBLEAndroidManifest.ts`: switch the feature-flag gate to `requiresBluetoothLeHardware`.

`example-expo/app.json`: `"requiresBluetoothLeHardware": true` (keep `modes: ["central"]`).

`docs/EXPO_PLUGIN.md`: rename the row; state it does **not** start a foreground service; warn that `neverForLocation` is a strong Android assertion and may filter some BLE beacons; restoration `clientId` example `com.example.app.ble-client`.

- [ ] **Step 3: Run**

```sh
pnpm test:plugin
```

Expected: PASS.

---

### Task B4: Example service lifecycle and SIG usage

**Files:** both `BLEService.ts` copies, both example READMEs, parity test.

Behavior to implement in **both** services:

1. **Identity:** module-level stable `clientId` (`bare-example-ble-client` / `expo-example-ble-client`) and `hostSessionScope`. `managerId` may include an incrementing instance number.
2. **`operation()`:** each user action creates `AbortController` + `deadline(manager.monotonicNow() + 15_000)` (or the UI-provided controller). Never `{ signal: null, deadline: null }`.
3. **Overflow:** `consumeScan` / `consumeNotification` handle `item.kind === 'overflow'` by recording loss to a callback or throwing `stream.overflow`. Do not drop it.
4. **Paths:** replace `findCharacteristicPath` `.find()` with `resolveCharacteristicPath`. Ambiguous UUID pairs become `gatt.ambiguous-path`.
5. **Optional features:** `readProfileValue` rethrows unless `error instanceof BackendContractError` and `error.code` is `gatt.not-found` or `gatt.property-not-supported`. Those two become `{ skipped: true, reason: error.code }`.
6. **Profiles:** do not `database.read` Temperature Measurement or Blood Pressure Measurement. Battery Level and Device Information strings remain reads. Temperature/blood pressure: omit, or subscribe-once via `firstNotification` if the UI already has a subscribe path; do not teach them as reads.
7. **`destroy()`:**
   - set `destroying = true`
   - `await this.managerCreation` if in flight
   - attempt `stopScan()`, notification `remove()`, `disconnect()`/`release()`, `manager.destroy()` independently
   - aggregate failures (array or `AggregateError`)
   - if `ensureManager` completes after `destroying`, immediately `manager.destroy()` and do not assign it as live
8. **README:** either show AbortSignal usage or stop claiming it.

Parity test: assert both files still export the same *method names* and both contain `overflow` handling, `resolveCharacteristicPath`, and do **not** contain `readHeartRateMeasurement` / Temperature Measurement `database.read` / Blood Pressure `database.read`. Do not require byte-identical source if host prefixes differ.

- [ ] **Step 1: Write/adjust failing parity assertions**
- [ ] **Step 2: Implement both services**
- [ ] **Step 3: Run**

```sh
pnpm exec jest __tests__/ExampleBleService.parity.test.js --runInBand
```

Expected: PASS.

---

### Task B5: Slice B validation

```sh
pnpm exec jest __tests__/profiles/standard-commands.test.js __tests__/PackageSurface4.test.js __tests__/ExampleBleService.parity.test.js --runInBand
pnpm test:plugin
pnpm lint
```

Expected: all pass.

Do not bump the package version in this slice.
