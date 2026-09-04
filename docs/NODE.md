<!-- docs/NODE.md -->

# Node.js

The root import does not open an adapter. Pick one backend:

| Import                                   | Host                                         |
| ---------------------------------------- | -------------------------------------------- |
| `unified-ble-manager/node/corebluetooth` | macOS                                        |
| `unified-ble-manager/node/winrt`         | Windows                                      |
| `unified-ble-manager/node/bluez`         | Linux (needs `dbus-next@^0.10.2` in the app) |

This source targets `4.0.22`. Tagged releases ship Node-API v8 prebuilds for macOS and Windows on `arm64` and `x64`. A normal install should not compile native code. BlueZ talks D-Bus and has no addon.

## One-call factories

```ts
import { createCoreBluetoothBleManager } from 'unified-ble-manager/node/corebluetooth'
import { createWinRtBleManager } from 'unified-ble-manager/node/winrt'
import { createBluezBleManager } from 'unified-ble-manager/node/bluez'

const manager = await createCoreBluetoothBleManager()
```

If there is no adapter, the factory throws `adapter.unavailable`. If more than one adapter exists and you omit `adapterId`, the factory picks the first adapter in a deterministic order (by adapter id) so the common single-adapter case needs no configuration and a multi-adapter host still selects the same controller every run; pass `adapterId` (e.g. a BlueZ path like `/org/bluez/hci1`) to target a specific controller — an unusual need, for example a second USB Bluetooth dongle used for debugging. Missing native artifacts throw `capability.unavailable` — there is no fallback to Noble, Web Bluetooth, or a simulator. Expected Node engines are those in `package.json`.

On `SIGINT`/`SIGTERM`, await `manager.destroy()`. Then scan/connect/GATT with the same `BleManager` helpers as React Native.

## Advanced provider construction

> **Maintainer/host-authoring reference — not ordinary application construction.**
> Use the one-call factories above for application code. This provider example
> is for maintainers implementing a host integration or authors wiring an
> explicitly selected backend; it is not the normal application recipe.

```ts
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } from 'unified-ble-manager/advanced'
import {
  coreBluetoothCompatibility,
  createNativeCoreBluetoothBackendProvider
} from 'unified-ble-manager/node/corebluetooth'

const now = () => performance.now()
const provider = createNativeCoreBluetoothBackendProvider({ now })
const adapters = await provider.listAdapters()
if (adapters[0] === undefined) {
  throw new Error('No CoreBluetooth adapter is available.')
}

const manager = await createBleManagerFromProvider(
  {
    provider,
    selection: { selectedAdapterId: adapters[0].adapterId },
    coreCompatibility: coreBluetoothCompatibility,
    manager: {
      clientId: 'node-corebluetooth-client',
      managerId: 'node-corebluetooth-manager',
      ownerMode: 'owning'
    }
  },
  { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
)
```

WinRT is the same shape with `createNativeWinRtBackendProvider` and `winRtCompatibility` from `unified-ble-manager/node/winrt`. BlueZ:

```ts
import { createDbusNextBluezBackendProvider } from 'unified-ble-manager/node/bluez'

const provider = createDbusNextBluezBackendProvider({ busKind: 'system', now })
```

Then scan and GATT through the same `BleManager` as React Native. Await `manager.destroy()` when the process session ends.

If you are on an unsupported architecture, the package still includes `node-gyp` sources as an explicit fallback. That is not the default path.

## Electron

Do not load a Node radio factory from a renderer. See [`ELECTRON.md`](ELECTRON.md).

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md), [`PLATFORMS.md`](PLATFORMS.md).
