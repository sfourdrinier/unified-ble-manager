// example-web/src/driver.ts
//
// Web Bluetooth host of the shared test driver (driver.html). The chooser
// needs a user gesture, so runs park in an explicit "awaiting-user-gesture"
// phase until a person clicks "Open chooser"; nothing synthesizes that click.

/// <reference types="vite/client" />

import { createWebBleManager } from 'unified-ble-manager/web'
import ubmPackage from 'unified-ble-manager/package.json'
import { bootBrowserDriver } from '../../examples-shared/driver/browser/boot.ts'
import { adapterHostManager, driverUrlFromQuery, driverUrlFromScriptUrl } from '../../examples-shared/driver/index.ts'
import '../../examples-shared/driver/browser/driver.css'

function resolveDriverUrl() {
  const explicit = driverUrlFromQuery(location.search)
  if (explicit !== null) return explicit
  const derived = driverUrlFromScriptUrl(location.href)
  return derived === null
    ? { url: null, reason: `page ${location.origin} was not served over http(s); open it with ?driver=ws://<mac>:8795/host` }
    : { url: derived, reason: 'derived from the page host' }
}

const mount = document.querySelector<HTMLElement>('#driver')
if (mount === null) throw new Error('driver.html has no #driver element')

const driver = bootBrowserDriver({
  host: 'web',
  backend: 'web/web-bluetooth',
  // Web Bluetooth has no background mode; the web backend reports it as web:background-operation.
  createManager: async () => adapterHostManager(await createWebBleManager(), 'web:background-operation'),
  requireUserGesture: true,
  driverUrl: resolveDriverUrl(),
  mount,
  appBuild: { ubmVersion: ubmPackage.version }
})

// Vite HMR: a hot update of this module must not orphan a run that still holds
// a connection. Vite awaits the returned promise; a cleanup failure is logged
// by disposeDriver and surfaces as a rejected dispose, never dropped.
import.meta.hot?.dispose(() => driver.dispose('hmr-dispose'))
