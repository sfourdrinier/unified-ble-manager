# Slice D — host factories, adapter state stream, and host guides

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Execute **D-code after Slice B**, then **D-docs together with Slice A**. Read `docs/superpowers/plans/2026-08-19-rc1-review-response.md` first.

**Goal:** Make normal host construction one call, expose adapter-state as a bounded stream, and write host guides that a developer can finish without inventing plumbing.

**Architecture:** Convenience factories wrap existing providers. `BleManager.adapterStates` is a public wrapper over backend `adapter.watchState()` (already on the contract). Host docs are written in the A+D-docs pass against this surface. Tauri implementation stays frozen.

**Tech Stack:** existing host entrypoints, `createBleManagerFromProvider`, backend `AdapterStateWatch`, public helpers.

---

## D-code vs D-docs

| Phase | When | What |
| --- | --- | --- |
| D-code | Step 2 of rc.1 (after B) | Factories, `adapterStates`, scan presets, cleanup helpers, Electron composition files. Minimal test-string updates only. |
| D-docs | Step 3 with Slice A | `docs/WEB.md`, `NODE.md`, `ELECTRON.md`, `ELECTRON_SECURITY_MODEL.md`, `TAURI.md` (read-only consume of IPC), GETTING_STARTED host notes |

---

## Files (D-code)

| File | Responsibility |
| --- | --- |
| `src/react-native-manager.ts` | Rename current factory to `WithEnvironment`; add app factory |
| `src/react-native.ts` | Export both |
| `src/web.ts` + `src/web/navigator-web-bluetooth-boundary.ts` | Optional `environment`; default from `globalThis` |
| `src/node-corebluetooth.ts` | `createCoreBluetoothBleManager` |
| `src/node-winrt.ts` | `createWinRtBleManager` |
| `src/node-bluez.ts` | `createBluezBleManager` |
| `src/manager/ble-manager.ts` + core | `adapterStates()` |
| `src/manager/public-helpers.ts` + `src/index.ts` | `defaultScanDelivery`, `scanForServices`, `withDiscoveredConnection`, `throwIfCleanupFailed` |
| `__tests__/manager/` and host tests | New behavior |
| `example-electron/main.js` `preload.js` `renderer.js` | Runnable composition (deterministic radio OK) |

**Frozen:** all Tauri/IPC paths listed in the master plan.

---

### Task D1: Tests for convenience factories

- [ ] **Step 1: RN factory tests**

Create `__tests__/react-native/createReactNativeBleManager.test.js` (or extend an existing RN construction test).

```js
const rn = require('unified-ble-manager/react-native')

test('app factory is exported', () => {
  expect(typeof rn.createReactNativeBleManager).toBe('function')
  expect(typeof rn.createReactNativeBleManagerWithEnvironment).toBe('function')
})
```

A unit test of `createReactNativeBleManagerWithEnvironment` must still accept `{ platform, control, now, clientId, managerId, hostSessionScope }` — that is today’s implementation, renamed.

A test of the app factory should stub `getNativeUnifiedBleProtocolControl` and `Platform.OS` if the repo already does that. If no RN runtime is available in Jest, test only:

- export presence
- that `createReactNativeBleManager({ clientId, managerId, hostSessionScope })` rejects empty `hostSessionScope` with `argument.invalid`
- that an unknown platform in `WithEnvironment` still fails closed

- [ ] **Step 2: Web factory tests**

Extend `__tests__/web/` (see `Web4.0Example.test.js` and existing web tests).

`createNavigatorWebBleManager({ clientId, managerId })` must work when `environment` is omitted **and** `globalThis.navigator.bluetooth` can be injected in the test harness.

`createNavigatorWebBleManager({ clientId, managerId, environment })` remains the test/injection path.

- [ ] **Step 3: Node factory export tests**

```js
const cb = require('unified-ble-manager/node/corebluetooth')
const winrt = require('unified-ble-manager/node/winrt')
const bluez = require('unified-ble-manager/node/bluez')
expect(typeof cb.createCoreBluetoothBleManager).toBe('function')
expect(typeof winrt.createWinRtBleManager).toBe('function')
expect(typeof bluez.createBluezBleManager).toBe('function')
```

A factory that cannot load a native boundary must throw `capability.unavailable` (already used by `createNativeCoreBluetoothBackendProvider`). Do not fall back to Noble, Web Bluetooth, or the deterministic backend.

- [ ] **Step 4: Run and confirm red**

```sh
pnpm exec jest __tests__/PackageSurface4.test.js --runInBand
```

Add assertions there too: root still does not create a radio; new functions exist on the host entrypoints only.

---

### Task D2: Implement convenience factories

**RN** (`src/react-native-manager.ts`):

```ts
export interface ReactNativeBleManagerAppOptions {
  readonly clientId: string
  readonly managerId: string
  readonly hostSessionScope: string
  readonly createOwnerId?: () => string
}

export async function createReactNativeBleManager(
  options: ReactNativeBleManagerAppOptions
): Promise<BleManager<string, NativeBackendIdentity<string>>> {
  return createReactNativeBleManagerWithEnvironment({
    ...options,
    platform: inferReactNativeBlePlatform(),
    control: getNativeUnifiedBleProtocolControl(),
    now: () => performance.now()
  })
}

function inferReactNativeBlePlatform(): ReactNativeBlePlatform {
  const os = require('react-native').Platform.OS
  if (os === 'android') return 'android'
  if (os === 'ios') return 'apple'
  throw contractError('argument.invalid', 'platform', 'react-native-manager.platform', {
    safeMessage: `Unsupported React Native platform: ${String(os)}`
  })
}
```

Rename the current function body to `createReactNativeBleManagerWithEnvironment` with today’s `ReactNativeBleManagerOptions` type (keep that type name for the injectable form).

Update `example/.../BLEService.ts` construction to the app factory (Slice B already has identity rules). Tests that passed `platform/control/now` switch to `WithEnvironment`.

**Web** (`src/web.ts`):

```ts
export interface NavigatorWebBleManagerOptions {
  readonly clientId: string
  readonly managerId: string
  readonly environment?: NavigatorWebBluetoothEnvironment
}
```

When `environment` is omitted, call `createDefaultNavigatorWebBluetoothEnvironment()` in `navigator-web-bluetooth-boundary.ts`:

- `bluetooth`: `globalThis.navigator?.bluetooth ?? null`
- `isSecureContext`: `globalThis.isSecureContext === true`
- `hasTransientUserActivation`: `globalThis.navigator?.userActivation?.isActive === true`
- `now`: `() => performance.now()`
- timers: `globalThis.setTimeout` / `clearTimeout`
- page lifecycle: `visibilitychange` + `pagehide` on `document`/`window` if present; no-op unsubscribe if not
- `implementationVersion` / `browserEngine`: existing constants or `'chromium'` / package implementation version already used by tests

**Node:**

```ts
export interface NodeBleManagerAppOptions {
  readonly clientId: string
  readonly managerId: string
  readonly now?: () => number
  readonly selectedAdapterId?: string
}

export async function createCoreBluetoothBleManager(options: NodeBleManagerAppOptions) {
  const now = options.now ?? (() => performance.now())
  const provider = createNativeCoreBluetoothBackendProvider({ now })
  const adapters = await provider.listAdapters()
  const selected = selectAdapter(adapters, options.selectedAdapterId)
  return createBleManagerFromProvider({
    provider,
    selection: { selectedAdapterId: selected.adapterId },
    coreCompatibility: coreBluetoothCompatibility,
    manager: { clientId: options.clientId, managerId: options.managerId, ownerMode: 'owning' }
  }, { ...DEFAULT_BLE_MANAGER_OPTIONS, now })
}
```

`selectAdapter`: if `selectedAdapterId` is set, find it or throw `adapter.unavailable`. If omitted and `adapters.length === 1`, use it. If `length === 0`, throw `adapter.unavailable`. If `length > 1`, throw `adapter.ambiguous` (do not pick silently).

Same shape for WinRT. BlueZ:

```ts
export async function createBluezBleManager(
  options: NodeBleManagerAppOptions & { readonly busKind?: 'system' | 'session' }
)
```

Default `busKind: 'system'`. Same adapter selection rules. Keep `createDbusNextBluezBackendProvider` / `createNative*` for advanced users.

Export the new functions from the existing host files only. Root `unified-ble-manager` still selects no radio.

Update `__tests__/PackageSurface4.test.js` exports lists if they enumerate host functions.

---

### Task D3: `BleManager.adapterStates`

Backend contract already has:

```ts
watchState(): Promise<AdapterStateWatch<Attachment>>
// AdapterStateWatch { initial, transitions: BoundedAsyncStream<AdapterStateSnapshot> }
```

- [ ] **Step 1: Failing test** using `DeterministicTestBackend`

```js
const session = await manager.adapterStates()
expect(session.initial.power).toBeDefined()
const next = []
const consume = (async () => {
  for await (const item of session.values) {
    if (item.kind === 'value') next.push(item.value)
    else break
  }
})()
// flip adapter power on the deterministic backend
await session.stop()
await consume
```

Also: `manager.destroy()` stops the watch (no leak). AbortSignal on options cancels the stream.

- [ ] **Step 2: Implement**

Add `UnifiedBleCore.adapterStates()` that calls `this.backend.adapter.watchState()`, projects `transitions` as `values`, and tracks the watch as an owned resource released in `destroy()`.

```ts
// BleManager
adapterStates(options?: { readonly signal?: AbortSignal }): Promise<{
  readonly initial: AdapterStateSnapshot<Attachment>
  readonly values: BoundedAsyncStream<AdapterStateSnapshot<Attachment>>
  stop(): Promise<CleanupRecord>
}>
```

Use the backend stream as-is. Do not poll `adapterState()`.

If core has no backend accessor for `adapter`, add the smallest private path. Do not change `watchState()` semantics.

- [ ] **Step 3: Run the new test — expect PASS**

---

### Task D4: Scan presets and cleanup helpers

**Files:** `src/manager/public-helpers.ts`, `src/index.ts`, helper tests (create `__tests__/manager/public-helpers.test.js` if missing).

```ts
export function defaultScanDelivery() {
  return Object.freeze({
    itemCapacity: capacity(32),
    byteCapacity: capacity(16 * 1024),
    reservedControlCapacity: capacity(2),
    overflowPolicy: 'drop-oldest'
  })
}

export function scanForServices<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  manager: BleManager<Attachment, Identity>,
  serviceUuids: readonly Uuid[],
  options: Omit<ScanUntilOptions<Attachment>, 'scan'> & {
    readonly scan?: Partial<ScanUntilOptions<Attachment>['scan']>
  }
) {
  const scan = options.scan ?? {}
  return scanUntil(manager, {
    matches: options.matches,
    scan: {
      filter: {
        serviceUuids,
        manufacturerData: scan.filter?.manufacturerData ?? [],
        localNamePrefix: scan.filter?.localNamePrefix ?? null
      },
      duplicatePolicy: scan.duplicatePolicy ?? 'merged',
      timestampPolicy: scan.timestampPolicy ?? 'source-then-receipt',
      delivery: scan.delivery ?? defaultScanDelivery(),
      deadline: scan.deadline,
      signal: scan.signal,
      sharing: scan.sharing ?? { mode: 'owner', allowSharing: false }
    }
  })
}

export async function withDiscoveredConnection<Attachment extends string, Identity extends BackendIdentity<Attachment>, Value>(
  manager: BleManager<Attachment, Identity>,
  peerId: PeerId<Attachment>,
  options: PublicOperationOptions,
  fn: (session: ConnectedGattDatabase<Attachment, Identity>) => Promise<Value>
): Promise<Value> {
  return withConnection(manager, peerId, options, async connection => {
    const database = await connection.discover(options)
    const snapshot = await database.snapshot()
    return fn(Object.freeze({ connection, database, snapshot }))
  })
}

export function throwIfCleanupFailed(cleanup: CleanupRecord, operation: string): void {
  if (cleanup.state !== 'release-failed') return
  throw contractError('lifecycle.invalid-state', 'cleanup', operation, {
    failures: cleanup.failures ?? []
  })
}
```

Inspect `CleanupRecord` and pass through `failures` exactly as typed. Do not stringify them away.

Export all four from `src/index.ts` / `src/manager/index.ts`.

Tests:

- `defaultScanDelivery()` is frozen and uses `drop-oldest`
- `scanForServices` forwards `AbortSignal` and stops the scan (reuse `scanUntil` tests)
- `withDiscoveredConnection` releases the connection on function throw **and** on success
- `throwIfCleanupFailed` on `{ state: 'released' }` is a no-op; on `release-failed` throws `BackendContractError` whose details include the structured failures

Do **not** add `scanForFirstMatch`. `find` / `scanUntil` remain the names.

---

### Task D5: Electron composition example (not live radio)

**Files:**
- Keep: `example-electron/smoke.js` (L1 packed smoke)
- Create: `example-electron/main.js`, `example-electron/preload.js`, `example-electron/renderer.js`
- Modify: `example-electron/README.md` (honest L1/composition wording only in D-code; full consumer guide is D-docs)

The composition app must demonstrate the **call sequence**, using `unified-ble-manager/testing` deterministic backend in CI (same as smoke). It must:

1. create provider + main `BleManager`
2. create router / binding
3. authenticate `WebContents` (or the existing test double the smoke already uses)
4. expose a narrow preload bridge (no raw `ipcRenderer`)
5. create `ElectronRendererBleClient`
6. scan → connect → discover → read or subscribe → release → destroy binding → destroy manager

If a real `BrowserWindow` cannot run in this repo’s Jest/CI, the files must still be valid Node that `smoke.js` or a new `example-electron/composition.js` can require. Do not load a Node-API radio in the renderer file.

`contextIsolation: true` and `nodeIntegration: false` must appear in the BrowserWindow options in `main.js` even if CI never opens a window.

---

### Task D6: D-code validation

```sh
pnpm exec jest __tests__/PackageSurface4.test.js __tests__/profiles/standard-commands.test.js --runInBand
pnpm exec jest __tests__/ExampleBleService.parity.test.js --runInBand
# plus the new factory / adapterStates / helper test files created above
pnpm lint
```

Expected: PASS.

---

## D-docs (execute with Slice A, not before)

Follow `2026-08-19-rc1-slice-a-docs.md` for README/GETTING_STARTED. Additional host pages:

### Task D7: `docs/WEB.md`

First example:

```ts
const session = await createNavigatorWebBleManager({
  clientId: 'web-app-ble-client',
  managerId: 'web-app-ble-manager'
})
```

Then: `chooser.choose()` **inside a click handler**; `getDevices()` for previously granted devices; Permissions Policy / iframe caveat; finite read or `firstNotification`; full `destroy()`. Keep environment injection as “tests / unusual hosts.”

### Task D8: `docs/NODE.md`

One complete journey per backend using `createCoreBluetoothBleManager` / `createWinRtBleManager` / `createBluezBleManager`. Include: adapter selection rules (1 / 0 / many), ESM + CJS import lines, expected Node engines from `package.json`, `SIGINT`/`destroy()`, `capability.unavailable` when the native addon cannot load. Troubleshooting: macOS Bluetooth permission, Windows prebuild ABI, Linux D-Bus / `dbus-next`. Keep `createBleManagerFromProvider` as advanced.

### Task D9: Electron docs split

- `docs/ELECTRON.md`: the 13-step runnable sequence pointing at `example-electron/main.js` + renderer. Use `createCoreBluetoothBleManager` (or Electron-main provider factory that already exists) in main only.
- Create `docs/ELECTRON_SECURITY_MODEL.md`: move main-frame auth, navigation cleanup, event ack, stream bounds, generation quarantine, VM membrane, unsupported threat claims.
- BrowserWindow: `contextIsolation: true`, `nodeIntegration: false`, no generic `ipcRenderer`, CSP note, ASAR unpack of native addons, signing/notarization as *app* responsibilities not library proof.

Do not call Electron “live-radio validated.”

### Task D10: `docs/TAURI.md` (consume-only)

Read `src/ipc/manager.ts` **as it exists at this moment**. Document the real `IpcBleManager` methods (scan options, overflow, `timeoutMs`, connect, discover, characteristic operations, destroy). If `IpcGattDatabase.snapshot` / path-based `read` exist and tests assert them, document that workflow. If they do not, document handle-based `IpcCharacteristic.read`.

Do not edit `src/ipc/**` or `example-tauri/**`. State Cargo path into `node_modules/.../native/tauri` and that it is the supported install in rc.1. Mention drop-oldest remote stream limits from `REMOTE_STREAM_LIMITS` in `src/ipc/manager.ts`.

If the file is mid-edit by the other engineer (dirty `src/ipc/manager.ts`), write `docs/TAURI.md` against exported types only and skip undocumented stub getters.

### Task D11: Product claim

Every host page and README: “one bytes-first model and lifecycle semantics; host-specific construction and ownership.” Never “same manager contract on every host.”
