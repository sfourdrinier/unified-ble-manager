'use strict'

/**
 * Narrow preload bridge. Do not expose ipcRenderer. The renderer only sees
 * these named transport methods. The public renderer factory consumes this
 * structural transport and owns the low-level client internally.
 */

const bridgeNames = Object.freeze([
  'invoke',
  'subscribe',
  'acknowledge'
])

module.exports = { bridgeNames }
