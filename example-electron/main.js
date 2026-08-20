'use strict'

/**
 * Electron main composition. This file is the documented owner of the radio.
 * CI may require it without opening a BrowserWindow. It does not load a native
 * addon in the renderer and is not live-radio evidence.
 */

const path = require('node:path')

const browserWindowWebPreferences = Object.freeze({
  contextIsolation: true,
  nodeIntegration: false,
  sandbox: true,
  preload: path.join(__dirname, 'preload.js')
})

async function createMainOwnedManager(createManager) {
  return createManager({
    clientId: 'electron-main-client',
    managerId: 'electron-main-manager'
  })
}

module.exports = {
  browserWindowWebPreferences,
  createMainOwnedManager
}
