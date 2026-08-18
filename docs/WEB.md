<!-- docs/WEB.md -->

# Web Bluetooth

Use `unified-ble-manager/web` in a secure context (HTTPS or localhost) from a user gesture. There is no background scan and no process-level restoration.

**Current package:** `4.0.0-rc.0`. The backend is Experimental until a bound live-radio receipt says otherwise. See [`PLATFORMS.md`](PLATFORMS.md).

## Create a matched chooser and manager

```ts
import { createNavigatorWebBleManager } from 'unified-ble-manager/web'

const timers = new Map<object, ReturnType<typeof window.setTimeout>>()

const session = await createNavigatorWebBleManager({
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
  },
  clientId: 'web-app-ble-client',
  managerId: 'web-app-ble-manager'
})
```

`session.manager` is the same host-neutral `BleManager` as other hosts. `session.chooser` is the browser picker.

## Choose a device, then connect

Call `chooser.choose()` from a click or other transient user activation. Then use the normal GATT loop.

```ts
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import { BATTERY_SERVICE } from 'unified-ble-manager/profiles/battery-service'
import { deadline } from 'unified-ble-manager'

const until = deadline(session.manager.monotonicNow() + 20_000)
const op = { signal: new AbortController().signal, deadline: until }

const selection = await session.chooser.choose(
  {
    filters: [{ serviceUuids: [HEART_RATE_SERVICE], manufacturerData: [], localNamePrefix: null }],
    acceptAllDevices: false,
    optionalServices: [HEART_RATE_SERVICE, BATTERY_SERVICE]
  },
  op
)

const connection = await session.manager.connect(selection.peerId, op)
const database = await connection.discover(op)
```

`session.manager.scan()` fails with `capability.unsupported`. Keep a real chooser button.

When the page session ends:

```ts
await session.manager.destroy()
```

A working Chrome harness lives in [`example-web/`](../example-web/). It is a live check, not a support claim.

## Maintainers

[`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
