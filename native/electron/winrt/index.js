// native/electron/winrt/index.js

'use strict'

const { loadNodeApiAddon } = require('../../load-node-api-addon')

if (process.platform !== 'win32') {
  throw new Error('WinRT contract boundary is Windows-only')
}

const nativeModule = loadNodeApiAddon({
  moduleDirectory: __dirname,
  addonName: 'unified_ble_winrt'
})
if (nativeModule === null) {
  throw new Error('WinRT contract boundary requires a package prebuild for this architecture or a local source build')
}
const boundaryVersion = 2
const requiredBoundaryMethods = [
  'listAdapters',
  'selectAdapter',
  'adapterSnapshot',
  'startScan',
  'stopScan',
  'connect',
  'disconnect',
  'discover',
  'read',
  'write',
  'readDescriptor',
  'writeDescriptor',
  'startNotify',
  'stopNotify',
  'onConnectionLost',
  'onDatabaseChanged',
  'onAdapterState',
  'onSecurityState',
  'onScanTerminal',
  'securityState',
  'pair',
  'cancelPairing',
  'unpair',
  'ingressTelemetry',
  'destroy'
]

if (nativeModule.boundaryVersion !== boundaryVersion || typeof nativeModule.createContractBoundary !== 'function') {
  throw new Error('The WinRT Node-API artifact does not implement strict native boundary protocol v2')
}

function createContractBoundary() {
  const boundary = nativeModule.createContractBoundary()
  if (boundary === null || typeof boundary !== 'object') {
    throw new Error('The WinRT native boundary factory did not return an object')
  }
  for (const method of requiredBoundaryMethods) {
    if (typeof boundary[method] !== 'function') {
      throw new Error(`The WinRT native boundary protocol v2 is missing required method ${method}`)
    }
  }
  return boundary
}

module.exports = Object.freeze({
  boundaryVersion,
  createContractBoundary
})
