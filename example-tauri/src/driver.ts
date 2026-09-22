// example-tauri/src/driver.ts
//
// Tauri host of the shared test driver (driver.html). The webview uses the
// Tauri IPC client (`createTauriBleManager`); the Rust plugin owns the radio.
// The control server runs on the same machine, so the default driver URL is
// local; `?driver=ws://…/host` overrides it.

/// <reference types="vite/client" />

import { createTauriBleManager } from 'unified-ble-manager/tauri'
import ubmPackage from 'unified-ble-manager/package.json'
import { bootBrowserDriver } from '../../examples-shared/driver/browser/boot.ts'
import { LOCAL_DRIVER_URL, adapterHostManager, driverUrlFromQuery } from '../../examples-shared/driver/index.ts'
import '../../examples-shared/driver/browser/driver.css'

const mount = document.querySelector<HTMLElement>('#driver')
if (mount === null) throw new Error('driver.html has no #driver element')

const driver = bootBrowserDriver({
  host: 'tauri',
  backend: 'tauri/btleplug-plugin',
  createManager: async () => adapterHostManager(await createTauriBleManager(), 'background:desktop-maintain-connection'),
  requireUserGesture: false,
  driverUrl: driverUrlFromQuery(location.search) ?? { url: LOCAL_DRIVER_URL, reason: 'default local control server' },
  mount,
  appBuild: { ubmVersion: ubmPackage.version }
})

// Vite HMR: a hot update of this module must not orphan a run that still holds
// a connection. Vite awaits the returned promise; a cleanup failure is logged
// by disposeDriver and surfaces as a rejected dispose, never dropped.
import.meta.hot?.dispose(() => driver.dispose('hmr-dispose'))
