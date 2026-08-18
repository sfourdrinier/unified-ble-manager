<!-- docs/NODE.md -->

# Node.js

The root import does not open an adapter. Pick one backend:

| Import | Host |
| --- | --- |
| `unified-ble-manager/node/corebluetooth` | macOS |
| `unified-ble-manager/node/winrt` | Windows |
| `unified-ble-manager/node/bluez` | Linux (needs `dbus-next@^0.10.2` in the app) |

**Current package:** `4.0.0-rc.0`. Published releases ship Node-API v8 prebuilds for macOS and Windows on `arm64` and `x64`. A normal install should not compile native code. BlueZ talks D-Bus and has no addon.

## Create a manager (macOS)

```ts
import { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } from 'unified-ble-manager'
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
