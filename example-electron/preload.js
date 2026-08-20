'use strict'

/**
 * Narrow preload bridge. Do not expose ipcRenderer. The renderer only sees
 * these named methods.
 */

const bridgeNames = Object.freeze([
  'initialize',
  'scan',
  'connect',
  'discover',
  'read',
  'subscribe',
  'release',
  'destroy'
])

module.exports = { bridgeNames }
