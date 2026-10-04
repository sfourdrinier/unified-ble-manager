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
const { trustedDesktopOptions } = require('../../example-node/trusted-options.cjs')
const { pathToFileURL } = require('node:url')
const { app, BrowserWindow, ipcMain, Menu, MenuItem } = require('electron')
const electronMain = require('unified-ble-manager/electron/main')
const { shutdownProcessSession } = require('./shutdown.cjs')
const { createProcessSession } = require('./process-session.cjs')
const { createProcessControls } = require('./process-controls.cjs')
const { createRecordingDirectory } = require('./recording-directory.cjs')
const { installRendererRecovery } = require('./renderer-recovery.cjs')
const { createProcessDispatch } = require('./process-dispatch.cjs')
const { installProcessBridge, installAuthenticatedAppHandler } = require('./process-bridge.cjs')

const BACKENDS = Object.freeze({
  corebluetooth: options => electronMain.createCoreBluetoothProcessHost(options),
  winrt: options => electronMain.createWinRtProcessHost(options),
  bluez: options => electronMain.createBluezProcessHost(options)
})
const DEFAULT_BACKEND = Object.freeze({ darwin: 'corebluetooth', win32: 'winrt', linux: 'bluez' })

function argument(name) {
  const index = process.argv.indexOf(`--${name}`)
  if (
    name === 'bluez-daemon-owner' &&
    index !== -1 &&
    (process.argv[index + 1] === undefined || process.argv[index + 1].startsWith('--'))
  )
    throw new Error('--bluez-daemon-owner requires an explicit stricter daemon unique-owner restriction')
  return index === -1 ? undefined : process.argv[index + 1]
}

function selectBackend() {
  const requested = argument('backend') ?? process.env.UBM_ELECTRON_BACKEND ?? DEFAULT_BACKEND[process.platform]
  if (requested === undefined || !Object.hasOwn(BACKENDS, requested)) {
    throw new Error(`backend must be one of ${Object.keys(BACKENDS).join(' | ')}; received ${String(requested)}`)
  }
  return requested
}

/** Identity comes only from host facts (the WebContents and its window), never from renderer payloads. */
function authenticate(event) {
  const window = BrowserWindow.fromWebContents(event.sender)
  return {
    authenticatedClientId: `electron-renderer-${event.sender.id}`,
    authenticatedWindowScope: `window-${window === null ? 'none' : window.id}`,
    authenticatedSessionScope: 'default-session',
    // Trusted reference-app authorization, never a renderer-supplied grant.
    // Custom ceremony is intentionally not granted by this system-only harness.
    securityPermissions: ['security:state', 'security:pair', 'security:cancel-pairing', 'security:unpair']
  }
}

async function start() {
  const backend = selectBackend()
  const directory = path.join(app.getPath('userData'), 'continuation-recordings', backend)
  const prepareDirectory = createRecordingDirectory(directory)
  const adapterId = argument('adapter')
  const options = trustedDesktopOptions(
    backend,
    adapterId,
    argument('bluez-daemon-owner') ?? process.env.UBM_BLUEZ_DAEMON_OWNER
  )
  let recordingStore = null
  const session = createProcessSession({
    createProcessHost: () => BACKENDS[backend](options),
    async openRecordings() {
      if (recordingStore === null) {
        const opening = (async () => {
          await prepareDirectory()
          const profile = electronMain.DESKTOP_RUST_CORE_PROFILES[backend]
          const binding = await electronMain.loadDesktopCoreBinding({
            platform: backend,
            operationPrefix: profile.operationPrefix
          })
          return binding.openRecordingStore(directory)
        })()
        recordingStore = opening.catch(error => {
          recordingStore = null
          throw error
        })
      }
      return recordingStore
    },
    createBinding(manager) {
      const router = new electronMain.ElectronMainBleRouter({
        manager,
        maximumMessageBytes: 256 * 1024,
        maximumOutstandingOperations: 32,
        maximumRetainedBytes: 4 * 1024 * 1024,
        publish: async () => {
          throw new Error('binding publisher not installed')
        }
      })
      let handler = null
      const port = {
        handle(channel, invoke) {
          if (channel !== electronMain.ELECTRON_BLE_IPC_CHANNEL) throw new Error('unexpected BLE channel')
          handler = invoke
        },
        removeHandler() {
          handler = null
        }
      }
      let binding
      try {
        binding = new electronMain.ElectronMainBleBinding({ router, port, authenticate })
        binding.install()
        return {
          invoke(event, request) {
            if (handler === null) throw new Error('BLE binding closed')
            return handler(event, request)
          },
          destroy: () => binding.destroy()
        }
      } catch (originalCause) {
        throw Object.assign(new Error('Electron binding initialization failed', { cause: originalCause }), {
          retryCleanup: () => (binding === undefined ? router.destroy() : binding.destroy())
        })
      }
    }
  })

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
  const documentPath = path.join(__dirname, 'dist', 'index.html')
  const documentUrl = pathToFileURL(documentPath).href
  const controls = createProcessControls(session, directory, prepareDirectory)
  installProcessBridge({
    ipcMain,
    window,
    documentUrl,
    dispatch: createProcessDispatch({ controls: async () => controls, recordings: () => session.recordings() })
  })
  installAuthenticatedAppHandler({
    ipcMain,
    window,
    documentUrl,
    channel: electronMain.ELECTRON_BLE_IPC_CHANNEL,
    validateRequest(request) {
      if (request === null || typeof request !== 'object' || Array.isArray(request))
        throw new Error('invalid BLE request')
    },
    dispatch: async (request, event) => (await session.binding()).invoke(event, request)
  })
  const query = { backend: `electron/${backend}` }
  const driverUrl = argument('driver-url') ?? process.env.UBM_DRIVER_URL
  if (driverUrl !== undefined) query.driver = driverUrl
  let shuttingDown = false
  let exitCode = 0
  const loadDocument = () => window.loadFile(documentPath, { query })
  const rendererRecovery = installRendererRecovery({
    window,
    load: loadDocument,
    isShuttingDown: () => shuttingDown,
    report(result) {
      if (result.state === 'failed') {
        console.error(
          '[example-electron] renderer recovery failed; process owner and backlog retained',
          result.details,
          result.error
        )
        window.setTitle('UBM renderer recovery failed — process owner and backlog retained')
      } else console.log('[example-electron] renderer recovery', result.state, result.details)
    }
  })
  const menu =
    Menu.getApplicationMenu() ??
    Menu.buildFromTemplate([
      ...(process.platform === 'darwin' ? [{ role: 'appMenu' }] : []),
      { role: 'fileMenu' },
      { role: 'editMenu' },
      { role: 'viewMenu' },
      { role: 'windowMenu' }
    ])
  menu.append(
    new MenuItem({
      label: 'Recovery',
      submenu: [
        {
          label: 'Recover renderer for handoff',
          click() {
            if (!rendererRecovery.retry())
              console.log(
                '[example-electron] renderer recovery not admitted: no pending crash, shutdown, or recovery already in progress'
              )
          }
        }
      ]
    })
  )
  Menu.setApplicationMenu(menu)
  // Keep the renderer and its separate handoff bridge alive while Quit is refused.
  window.on('close', event => {
    event.preventDefault()
    app.quit()
  })
  const requestShutdown = () => {
    if (shuttingDown) return
    shuttingDown = true
    let released = false
    // Start known cleanup without waiting for a held initializer. Keep exact
    // failed owners and the independent data-handoff bridge available for retry.
    return shutdownProcessSession(session)
      .then(result => {
        for (const outcome of result.outcomes) {
          if ('error' in outcome) console.error('[example-electron]', outcome.name, outcome.error)
          else console.log('[example-electron]', outcome.name, JSON.stringify(outcome.receipt))
        }
        if (result.state === 'released') {
          released = true
          app.exit(exitCode)
        } else console.error('[example-electron] shutdown remains owned; quit again to retry')
      })
      .catch(error => {
        console.error('[example-electron] shutdown failed; quit again to retry', error)
      })
      .finally(() => {
        shuttingDown = false
        if (!released) rendererRecovery.resume()
      })
  }
  app.on('before-quit', event => {
    event.preventDefault()
    return requestShutdown()
  })
  try {
    await loadDocument()
  } catch (error) {
    // A renderer can already have acquired ownership before loadFile rejects.
    // Preserve the original failed-start exit code, but never bypass cleanup.
    exitCode = 1
    console.error('[example-electron] window failed to load', error)
    await requestShutdown()
  }
}

app.on('window-all-closed', () => app.quit())
app
  .whenReady()
  .then(start)
  .catch(error => {
    console.error('[example-electron] failed to start', error)
    app.exit(1)
  })
