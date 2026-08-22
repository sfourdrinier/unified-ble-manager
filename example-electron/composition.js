'use strict'

const assert = require('node:assert/strict')
const main = require('./composition-main')
const preload = require('./composition-preload')
const renderer = require('./composition-renderer')

assert.equal(main.browserWindowWebPreferences.contextIsolation, true)
assert.equal(main.browserWindowWebPreferences.nodeIntegration, false)
assert.equal(typeof main.browserWindowWebPreferences.preload, 'string')
assert.deepEqual(preload.bridgeNames, [
  'invoke',
  'subscribe',
  'acknowledge'
])
assert.equal(main.mainEntrypoint, 'unified-ble-manager/electron/main')
assert.ok(main.mainExports.includes('ElectronMainBleRouter'))
assert.equal(renderer.rendererEntrypoint, 'unified-ble-manager/electron/renderer')
assert.equal(renderer.managerExport, 'createElectronRendererBleManager')
assert.ok(renderer.runRendererJourney.toString().includes('createManager({ transport })'))
assert.ok(renderer.runRendererJourney.toString().includes('manager.scan()'))
assert.ok(renderer.runRendererJourney.toString().includes('manager.destroy()'))

console.log('example-electron composition OK')
