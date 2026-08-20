'use strict'

/**
 * Narrow preload bridge. Do not expose ipcRenderer. The renderer only sees
 * these named methods.
 */

const bridgeNames = Object.freeze([
  'initialize',
  'request',
  'subscribeConnectionEvents',
  'destroy'
])

module.exports = { bridgeNames }
