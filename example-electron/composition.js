'use strict'

const assert = require('node:assert/strict')
const main = require('./composition-main')
const preload = require('./composition-preload')
const renderer = require('./composition-renderer')

assert.equal(main.browserWindowWebPreferences.contextIsolation, true)
assert.equal(main.browserWindowWebPreferences.nodeIntegration, false)
assert.equal(typeof main.browserWindowWebPreferences.preload, 'string')
assert.deepEqual(preload.bridgeNames, [
  'initialize',
  'request',
  'subscribeConnectionEvents',
  'destroy'
])
assert.equal(main.mainEntrypoint, 'unified-ble-manager/electron/main')
assert.ok(main.mainExports.includes('ElectronMainBleRouter'))
assert.equal(renderer.rendererEntrypoint, 'unified-ble-manager/electron/renderer')
assert.equal(renderer.clientExport, 'ElectronRendererBleClient')
assert.ok(renderer.runRendererJourney.toString().includes('client.initialize()'))
assert.ok(renderer.runRendererJourney.toString().includes('client.request'))
assert.ok(!renderer.runRendererJourney.toString().includes('client.scan('))

console.log('example-electron composition OK')
