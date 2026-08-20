'use strict'

const assert = require('node:assert/strict')
const main = require('./composition-main')
const preload = require('./composition-preload')
const renderer = require('./composition-renderer')

assert.equal(main.browserWindowWebPreferences.contextIsolation, true)
assert.equal(main.browserWindowWebPreferences.nodeIntegration, false)
assert.equal(typeof main.browserWindowWebPreferences.preload, 'string')
assert.ok(preload.bridgeNames.includes('scan'))
assert.equal(renderer.rendererEntrypoint, 'unified-ble-manager/electron/renderer')
assert.equal(renderer.clientExport, 'ElectronRendererBleClient')

console.log('example-electron composition OK')
