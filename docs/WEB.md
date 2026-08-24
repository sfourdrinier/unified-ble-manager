<!-- docs/WEB.md -->

# Web Bluetooth

Use `unified-ble-manager/web` in a secure context (HTTPS or localhost) from a user gesture. There is no background scan and no process-level restoration.

The current package is `4.0.0`. The backend is Experimental until artifact-bound physical-hardware validation says otherwise. See [`PLATFORMS.md`](PLATFORMS.md).

## Create the manager

```ts
import { createWebBleManager } from 'unified-ble-manager/web'

const ble = await createWebBleManager()
```

Call `ble.choose()` from a click handler. Chromium's
`navigator.bluetooth.getDevices()` can return previously granted devices; this
package does not wrap that browser API. Iframes need the `bluetooth`
Permissions Policy. Then connect and use the generation-bound GATT objects.

Inject `environment` only for tests or unusual hosts:

```ts
import { createWebBleManagerWithEnvironment } from 'unified-ble-manager/web'

const timers = new Map<object, ReturnType<typeof window.setTimeout>>()

const ble = await createWebBleManagerWithEnvironment({
  environment: {
    implementationVersion: '1',
    browserEngine: navigator.userAgent,
    bluetooth: {
      getAvailability:
        typeof navigator.bluetooth?.getAvailability === 'function'
          ? () => navigator.bluetooth.getAvailability()
          : undefined,
      requestDevice: options => navigator.bluetooth.requestDevice(options)
    },
    isSecureContext: () => window.isSecureContext,
    hasTransientUserActivation: () => navigator.userActivation?.isActive ?? false,
    now: () => performance.now(),
    setTimer: (callback, delayMilliseconds) => {
      const handle = Object.freeze({})
      const timer = window.setTimeout(() => {
        timers.delete(handle)
        callback()
      }, delayMilliseconds)
      timers.set(handle, timer)
      return handle
    },
    clearTimer: handle => {
      const timer = timers.get(handle)
      if (timer === undefined) {
        return
      }
      timers.delete(handle)
      window.clearTimeout(timer)
    },
    addPageLifecycleListener: listener => {
      const onVisibilityChange = () => {
        if (document.visibilityState === 'hidden') listener('page-hidden')
      }
      const onPageHide = () => listener('page-unloaded')
      document.addEventListener('visibilitychange', onVisibilityChange)
      window.addEventListener('pagehide', onPageHide)
      return () => {
        document.removeEventListener('visibilitychange', onVisibilityChange)
        window.removeEventListener('pagehide', onPageHide)
      }
    }
  }
})
```

`ble` is the same host-neutral `BleManager` as other hosts. `ble.choose()` is the browser picker.

## Choose a device, then connect

Call `ble.choose()` from a click or other transient user activation. Then use the normal GATT loop.

```ts
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import { BATTERY_SERVICE } from 'unified-ble-manager/profiles/battery-service'
import { createWebBleManager } from 'unified-ble-manager/web'

const ble = await createWebBleManager()

const peer = await ble.choose({
  filters: [{ serviceUuids: [HEART_RATE_SERVICE] }],
  optionalServices: [BATTERY_SERVICE],
  timeoutMs: 20_000
})

const connection = await ble.connect(peer, { timeoutMs: 15_000 })
const database = await connection.discover({ timeoutMs: 15_000 })
const battery = database.characteristic(BATTERY_SERVICE, '2A19')
const bytes = await battery.read({ timeoutMs: 10_000 })
```

`ble.scan()` fails with `capability.unsupported`. Keep a real chooser button.

When the page session ends:

```ts
await ble.destroy()
```

A working Chrome harness lives in [`example-web/`](../example-web/). It is a live check, not a support claim.

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
