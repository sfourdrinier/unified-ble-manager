// example-electron/driver/src/renderer.ts
//
// Electron renderer host of the shared test driver. It sees only the preload
// transport and builds the public manager with
// createElectronRendererBleManager; main owns the radio. The backend label and
// an optional driver URL arrive as query parameters set by main.

/// <reference types="vite/client" />

import { createElectronRendererBleManager, type ElectronRendererIpcTransport } from 'unified-ble-manager/electron/renderer'
import ubmPackage from 'unified-ble-manager/package.json'
import { bootBrowserDriver } from '../../../examples-shared/driver/browser/boot.ts'
import { LOCAL_DRIVER_URL, adapterHostManager, driverUrlFromQuery } from '../../../examples-shared/driver/index.ts'
import '../../../examples-shared/driver/browser/driver.css'

declare global {
  interface Window {
    readonly ubmElectronTransport: ElectronRendererIpcTransport<string, string>
  }
}

const mount = document.querySelector<HTMLElement>('#driver')
if (mount === null) throw new Error('index.html has no #driver element')
const backend = new URLSearchParams(location.search).get('backend')
if (backend === null) throw new Error('main did not pass the backend it selected (?backend=)')

const driver = bootBrowserDriver({
  host: 'electron',
  backend,
  createManager: async () => adapterHostManager(await createElectronRendererBleManager({ transport: window.ubmElectronTransport }), 'background:desktop-maintain-connection'),
  requireUserGesture: false,
  driverUrl: driverUrlFromQuery(location.search) ?? { url: LOCAL_DRIVER_URL, reason: 'default local control server' },
  mount,
  appBuild: { ubmVersion: ubmPackage.version, electronRenderer: navigator.userAgent }
})

// Vite HMR: a hot update of this module must not orphan a run that still holds
// a connection. Vite awaits the returned promise; a cleanup failure is logged
// by disposeDriver and surfaces as a rejected dispose, never dropped.
import.meta.hot?.dispose(() => driver.dispose('hmr-dispose'))
