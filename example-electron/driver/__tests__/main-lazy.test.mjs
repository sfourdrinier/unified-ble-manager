import { test } from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import vm from 'node:vm'
import { EventEmitter } from 'node:events'
import { createRequire } from 'node:module'
const require = createRequire(import.meta.url)

for (const failLoad of [false, true])
  for (const backend of ['corebluetooth', 'bluez'])
    test(
      `actual main shares ownership and preserves handoff; backend=${backend}, load failure=${failLoad}`,
      { timeout: 2000 },
      async () => {
        const handlers = new Map()
        const hooks = new Map()
        const exits = []
        const menuItems = [{ label: 'Existing menu' }]
        let opens = 0
        let borrowed = 0
        let closed = 0
        let directoryPreparations = 0
        let continuationStatus = null
        let enteredClose
        let finishClose
        const closing = new Promise(resolve => {
          enteredClose = resolve
        })
        const held = new Promise(resolve => {
          finishClose = resolve
        })
        let finishLoad
        const loaded = new Promise(resolve => {
          finishLoad = resolve
        })
        let rejectLoad
        const loadOutcome = failLoad
          ? new Promise((_resolve, reject) => {
              rejectLoad = reject
            })
          : Promise.resolve()
        const receipt = () => ({ state: 'released', failures: [] })
        const host = {
          createInternalManager: async () => {
            borrowed++
            return { destroy: async () => receipt() }
          },
          continuation: {
            status: async () => continuationStatus,
            recordings: async directory => {
              assert.equal(directory, `/private/app/continuation-recordings/${backend}`)
              assert.ok(directoryPreparations > 0, 'live trusted directory must exist')
            }
          },
          continuationAccess: {
            describeBacklog: async () => JSON.stringify({ ok: true, value: continuationStatus }),
            execute: async () => JSON.stringify({ ok: true, value: { started: true } })
          },
          destroy: async () => {
            closed++
            if (closed === 1) {
              enteredClose()
              await held
              return { state: 'release-failed', failures: ['refused'] }
            }
            return receipt()
          }
        }
        let window
        const loads = []
        class Window extends EventEmitter {
          constructor() {
            super()
            window = this
            this.destroyed = false
            const sender = new EventEmitter()
            Object.defineProperty(this, 'webContents', {
              get: () => {
                if (this.destroyed) throw new TypeError('Object has been destroyed')
                return sender
              }
            })
            this.webContents.mainFrame = { processId: 7, routingId: 11, url: 'file:///fixture/dist/index.html' }
            this.webContents.isDestroyed = () => this.destroyed
          }
          isDestroyed() {
            return this.destroyed
          }
          destroy() {
            this.destroyed = true
            this.emit('closed')
          }
          setTitle() {}
          async loadFile(file, options) {
            loads.push({ file, options })
            if (loads.length > 1)
              this.webContents.mainFrame = { processId: 8, routingId: 12, url: 'file:///fixture/dist/index.html' }
            this.webContents.emit('did-finish-load')
            finishLoad()
            await loadOutcome
          }
          static fromWebContents() {
            return window
          }
        }
        class Binding {
          constructor({ port }) {
            this.port = port
          }
          install() {
            this.port.handle('ble', async () => ({ kind: 'ok' }))
          }
          async destroy() {
            this.port.removeHandler('ble')
            return receipt()
          }
        }
        const app = {
          getPath: () => '/private/app',
          whenReady: () => Promise.resolve(),
          on: (name, fn) => hooks.set(name, fn),
          exit: code => {
            window.destroy()
            exits.push(code)
          },
          quit() {}
        }
        const api = {
          [backend === 'bluez' ? 'createBluezProcessHost' : 'createCoreBluetoothProcessHost']: async options => {
            assert.equal(
              JSON.stringify(options),
              JSON.stringify(
                backend === 'bluez' ? { connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: ':1.42' } } : {}
              )
            )
            opens++
            return host
          },
          ElectronMainBleRouter: class {},
          ElectronMainBleBinding: Binding,
          ELECTRON_BLE_IPC_CHANNEL: 'ble'
        }
        api.DESKTOP_RUST_CORE_PROFILES = { [backend]: { operationPrefix: 'direct-gatt' } }
        api.loadDesktopCoreBinding = async () => ({
          openRecordingStore: async directory => {
            assert.equal(directory, `/private/app/continuation-recordings/${backend}`)
            assert.ok(directoryPreparations > 0, 'offline trusted directory must exist')
            return { status: async () => JSON.stringify({ ok: true, value: { retained: true } }) }
          }
        })
        vm.runInNewContext(fs.readFileSync(new URL('../main.cjs', import.meta.url), 'utf8'), {
          require: name => {
            if (name === 'electron')
              return {
                app,
                BrowserWindow: Window,
                Menu: { getApplicationMenu: () => ({ append: item => menuItems.push(item) }), setApplicationMenu() {} },
                MenuItem: class {
                  constructor(options) {
                    Object.assign(this, options)
                  }
                },
                ipcMain: {
                  handle: (channel, fn) => handlers.set(channel, fn),
                  removeHandler: channel => handlers.delete(channel)
                }
              }
            if (name === 'unified-ble-manager/electron/main') return api
            if (name === '../../example-node/trusted-options.cjs')
              return require('../../../example-node/trusted-options.cjs')
            if (name === './recording-directory.cjs')
              return {
                createRecordingDirectory: directory => {
                  assert.equal(directory, `/private/app/continuation-recordings/${backend}`)
                  return async () => {
                    directoryPreparations++
                  }
                }
              }
            if (name.startsWith('./')) return require(`../${name.slice(2)}`)
            if (name.startsWith('node:')) return require(name)
            throw new Error(`unexpected import ${name}`)
          },
          process: {
            argv: ['--backend', backend, ...(backend === 'bluez' && failLoad ? ['--bluez-daemon-owner', ':1.42'] : [])],
            env: backend === 'bluez' ? { UBM_BLUEZ_DAEMON_OWNER: failLoad ? ':1.99' : ':1.42' } : {},
            platform: 'darwin'
          },
          __dirname: '/fixture',
          performance,
          console,
          URL,
          Buffer
        })
        await loaded
        assert.equal(opens, 0, 'boot never opens radio')
        const event = {
          sender: window.webContents,
          senderFrame: window.webContents.mainFrame,
          processId: 7,
          frameId: 11
        }
        const processInvoke = handlers.get('ubm-reference-process/1')
        assert.equal(JSON.parse(await processInvoke(event, { operation: 'status', args: {} })).value, null)
        assert.equal(opens, 0)
        await processInvoke(event, { operation: 'recording-status', args: { id: 'retained' } })
        assert.equal(opens, 0, 'offline directory preparation must not acquire a central')
        await processInvoke(event, {
          operation: 'execute',
          args: { peerId: 'peer', declarationJson: JSON.stringify({ recording: { id: 'live' } }) }
        })
        assert.equal(directoryPreparations, 2, 'both paths use the same trusted preparation boundary')
        await handlers.get('ble')(event, { kind: 'bootstrap' })
        await handlers.get('ble')(event, { kind: 'bootstrap' })
        assert.equal(opens, 1)
        assert.equal(borrowed, 1)
        continuationStatus = { queuedData: 0, lastError: null, continuationOutcome: null }
        if (!failLoad) {
          const reloaded = new Promise(resolve => window.webContents.once('did-finish-load', resolve))
          window.webContents.emit('render-process-gone', {}, { reason: 'killed', exitCode: 9 })
          await assert.rejects(processInvoke(event, { operation: 'status', args: {} }), /unauthorized/)
          await reloaded
          assert.equal(loads.length, 2)
          assert.equal(
            JSON.stringify(loads[0]),
            JSON.stringify(loads[1]),
            'reload must retain exact trusted document and query'
          )
          const newEvent = {
            sender: window.webContents,
            senderFrame: window.webContents.mainFrame,
            processId: 8,
            frameId: 12
          }
          assert.notEqual(JSON.parse(await processInvoke(newEvent, { operation: 'status', args: {} })).value, null)
          await assert.rejects(processInvoke(event, { operation: 'status', args: {} }), /unauthorized/)
          Object.assign(event, newEvent)
          assert.equal(opens, 1, 'renderer recovery must not acquire another native owner')
          assert.equal(closed, 0, 'renderer recovery must not shut down native collection')
          window.webContents.emit('render-process-gone', {}, { reason: 'crashed', exitCode: 1 })
          const manual = menuItems
            .find(item => item.label === 'Recovery')
            ?.submenu.find(item => item.label === 'Recover renderer for handoff')
          assert.ok(manual, 'trusted main recovery menu must remain available after automatic budget exhaustion')
          const manuallyReloaded = new Promise(resolve => window.webContents.once('did-finish-load', resolve))
          manual.click()
          manual.click()
          await manuallyReloaded
          assert.equal(loads.length, 3, 'repeated menu clicks coalesce into one navigation')
          assert.equal(menuItems[0].label, 'Existing menu')
          assert.equal(opens, 1)
          assert.equal(closed, 0)
          Object.assign(event, { senderFrame: window.webContents.mainFrame, processId: 8, frameId: 12 })
        }
        const quit = hooks.get('before-quit')
        let prevented = 0
        const quitEvent = {
          preventDefault() {
            prevented++
          }
        }
        const firstQuit = failLoad ? (rejectLoad(new Error('window load failed')), undefined) : quit(quitEvent)
        await closing
        await quit(quitEvent)
        assert.equal(closed, 1, 'overlapping Quit cannot bypass or duplicate cleanup')
        finishClose()
        if (failLoad) {
          // Let the failed startup's retained shutdown settle before the retry.
          await new Promise(resolve => setImmediate(resolve))
        } else await firstQuit
        assert.deepEqual(exits, [])
        assert.equal(closed, 1)
        await quit(quitEvent)
        assert.deepEqual(exits, [], 'confirmed radio release still requires data handoff')
        assert.equal(closed, 2)
        assert.notEqual(JSON.parse(await processInvoke(event, { operation: 'status', args: {} })).value, null)
        continuationStatus = null // Explicit renderer handoff completed; status is authoritative.
        await quit(quitEvent)
        assert.deepEqual(exits, [failLoad ? 1 : 0])
        assert.equal(closed, 2, 'confirmed owner is not redundantly destroyed')
        assert.equal(prevented, failLoad ? 3 : 4)
      }
    )
