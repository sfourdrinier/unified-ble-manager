// Electron host adapter: the preload exposes exactly the structural renderer
// transport on the versioned channel the main binding installs, and nothing
// else (no ipcRenderer, no Node API).

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import vm from 'node:vm'

const require = createRequire(import.meta.url)
const source = readFileSync(new URL('../preload.cjs', import.meta.url), 'utf8')

function loadPreload() {
  const exposed = {}
  const calls = []
  const listeners = new Map()
  const electron = {
    contextBridge: { exposeInMainWorld: (name, api) => (exposed[name] = api) },
    ipcRenderer: {
      invoke: async (channel, request) => (calls.push({ channel, request }), { kind: 'ok' }),
      on: (channel, listener) => listeners.set(listener, channel),
      removeListener: (channel, listener) => listeners.delete(listener)
    }
  }
  vm.runInNewContext(source, { require: name => (name === 'electron' ? electron : assert.fail(`preload required ${name}`)) })
  return { exposed, calls, listeners }
}

test('preload exposes only invoke, subscribe and acknowledge', () => {
  const { exposed } = loadPreload()
  assert.deepEqual(Object.keys(exposed), ['ubmElectronTransport'])
  assert.deepEqual(Object.keys(exposed.ubmElectronTransport).sort(), ['acknowledge', 'invoke', 'subscribe'])
})

test('the channel is the one the main binding installs, and subscribe can be undone', async () => {
  const { ELECTRON_BLE_IPC_CHANNEL } = require('unified-ble-manager/electron/renderer')
  const { exposed, calls, listeners } = loadPreload()
  const transport = exposed.ubmElectronTransport
  await transport.invoke({ kind: 'bootstrap' })
  await transport.acknowledge({ id: 'lease' }, 'e1')
  assert.deepEqual(calls.map(call => call.channel), [ELECTRON_BLE_IPC_CHANNEL, ELECTRON_BLE_IPC_CHANNEL])
  assert.equal(JSON.stringify(calls[1].request), JSON.stringify({ kind: 'event.ack', rendererLease: { id: 'lease' }, eventId: 'e1' }))
  const received = []
  const unsubscribe = transport.subscribe(event => received.push(event))
  const [[forward, channel]] = [...listeners]
  assert.equal(channel, ELECTRON_BLE_IPC_CHANNEL)
  forward({}, { kind: 'event', id: 1 })
  assert.deepEqual(received, [{ kind: 'event', id: 1 }])
  unsubscribe()
  assert.equal(listeners.size, 0)
})
