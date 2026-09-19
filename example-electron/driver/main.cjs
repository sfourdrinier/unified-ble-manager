'use strict'

/**
 * example-electron/driver/main.cjs
 *
 * Electron host of the shared test driver. Main owns the radio: it selects
 * one desktop backend explicitly (--backend / UBM_ELECTRON_BACKEND, else the
 * OS default), creates the main-process manager, and serves renderers only
 * through ElectronMainBleRouter + ElectronMainBleBinding on the versioned IPC
 * channel. The renderer (dist/, built by vite.config.mts) runs the shared
 * scenarios over createElectronRendererBleManager and never loads an addon.
 *
 *   pnpm prepack
 *   pnpm exec vite build --config example-electron/driver/vite.config.mts
 *   pnpm exec electron example-electron/driver/main.cjs [--backend corebluetooth|winrt|bluez] [--driver-url ws://…/host]
 */

const path = require('node:path')
const { app, BrowserWindow, ipcMain } = require('electron')
const { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } = require('unified-ble-manager/advanced')
const electronMain = require('unified-ble-manager/electron/main')

const BACKENDS = Object.freeze({
  corebluetooth: () => ({ provider: electronMain.createElectronMainCoreBluetoothBackendProvider, compatibility: electronMain.coreBluetoothCompatibility }),
  winrt: () => ({ provider: electronMain.createElectronMainWinRtBackendProvider, compatibility: electronMain.winRtCompatibility }),
  bluez: () => ({ provider: electronMain.createElectronMainBluezBackendProvider, compatibility: electronMain.bluezCompatibility })
})
const DEFAULT_BACKEND = Object.freeze({ darwin: 'corebluetooth', win32: 'winrt', linux: 'bluez' })

function argument(name) {
  const index = process.argv.indexOf(`--${name}`)
  return index === -1 ? undefined : process.argv[index + 1]
}

function selectBackend() {
  const requested = argument('backend') ?? process.env.UBM_ELECTRON_BACKEND ?? DEFAULT_BACKEND[process.platform]
  if (requested === undefined || !Object.hasOwn(BACKENDS, requested)) {
    throw new Error(`backend must be one of ${Object.keys(BACKENDS).join(' | ')}; received ${String(requested)}`)
  }
  return requested
}

async function createMainManager(backend) {
  const now = () => performance.now()
  const { provider: createProvider, compatibility } = BACKENDS[backend]()
  const provider = createProvider({ now })
  const adapters = await provider.listAdapters()
  const adapter = adapters[0]
  if (adapter === undefined) throw new Error(`${backend}: no Bluetooth adapter is available`)
  return createBleManagerFromProvider(
    {
      provider,
      selection: { selectedAdapterId: adapter.adapterId },
      coreCompatibility: compatibility,
      manager: { clientId: 'electron-main-client', managerId: 'electron-main-manager', ownerMode: 'owning' }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
  )
}

/** Identity comes only from host facts (the WebContents and its window), never from renderer payloads. */
function authenticate(event) {
  const window = BrowserWindow.fromWebContents(event.sender)
  return {
    authenticatedClientId: `electron-renderer-${event.sender.id}`,
    authenticatedWindowScope: `window-${window === null ? 'none' : window.id}`,
    authenticatedSessionScope: 'default-session'
  }
}

async function start() {
  const backend = selectBackend()
  const manager = await createMainManager(backend)
  const router = new electronMain.ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 256 * 1024,
    maximumOutstandingOperations: 32,
    maximumRetainedBytes: 4 * 1024 * 1024,
    publish: async () => {
      throw new Error('ElectronMainBleBinding installs the authenticated event publisher')
    }
  })
  const binding = new electronMain.ElectronMainBleBinding({ router, port: ipcMain, authenticate })
  binding.install()

  const window = new BrowserWindow({
    width: 1100,
    height: 900,
    title: `UBM test driver · electron/${backend}`,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      preload: path.join(__dirname, 'preload.cjs')
    }
  })
  const query = { backend: `electron/${backend}` }
  const driverUrl = argument('driver-url') ?? process.env.UBM_DRIVER_URL
  if (driverUrl !== undefined) query.driver = driverUrl
  await window.loadFile(path.join(__dirname, 'dist', 'index.html'), { query })

  let shuttingDown = false
  app.on('before-quit', event => {
    if (shuttingDown) return
    shuttingDown = true
    event.preventDefault()
    // Binding before manager, as docs/ELECTRON.md requires; each record is reported, not dropped.
    binding
      .destroy()
      .then(record => console.log('[example-electron] binding.destroy', JSON.stringify(record)))
      .catch(error => console.error('[example-electron] binding.destroy failed', error))
      .then(() => manager.destroy())
      .then(record => console.log('[example-electron] manager.destroy', JSON.stringify(record)))
      .catch(error => console.error('[example-electron] manager.destroy failed', error))
      .finally(() => app.exit(0))
  })
}

app.on('window-all-closed', () => app.quit())
app
  .whenReady()
  .then(start)
  .catch(error => {
    console.error('[example-electron] failed to start', error)
    app.exit(1)
  })
