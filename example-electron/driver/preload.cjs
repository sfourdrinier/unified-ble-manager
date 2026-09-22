'use strict'

/**
 * example-electron/driver/preload.cjs
 *
 * The narrow bridge: exactly the structural ElectronRendererIpcTransport
 * (invoke, subscribe, acknowledge) on the versioned channel, never
 * ipcRenderer itself.
 */

const { contextBridge, ipcRenderer } = require('electron')

const CHANNEL = 'unified-ble-manager:v2'

contextBridge.exposeInMainWorld('ubmElectronTransport', {
  invoke: request => ipcRenderer.invoke(CHANNEL, request),
  subscribe(listener) {
    const forward = (_event, message) => listener(message)
    ipcRenderer.on(CHANNEL, forward)
    return () => ipcRenderer.removeListener(CHANNEL, forward)
  },
  acknowledge: (rendererLease, eventId) => ipcRenderer.invoke(CHANNEL, { kind: 'event.ack', rendererLease, eventId })
})
