'use strict'

/**
 * example-electron/driver/preload.cjs
 *
 * Two narrow bridges: the structural ElectronRendererIpcTransport and named
 * application-owned continuation controls. Neither exposes ipcRenderer or
 * filesystem configuration.
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

// Application-owned continuation survives renderer teardown. Only named,
// ID-based controls cross this bridge; directory configuration stays in main.
const processControl = (operation, args) => ipcRenderer.invoke('ubm-reference-process/1', { operation, args })
contextBridge.exposeInMainWorld('ubmProcessControl', {
  execute: (peerId, declarationJson) => processControl('execute', { peerId, declarationJson }),
  describeBacklog: () => processControl('status', {}),
  prepareClaim: (maxItems, maxBytes) => processControl('prepare-claim', { maxItems, maxBytes }),
  acknowledgeClaim: token => processControl('acknowledge-claim', { token }),
  recordings: {
    status: id => processControl('recording-status', { id }),
    prepare: (id, maxItems, maxBytes) => processControl('recording-prepare', { id, maxItems, maxBytes }),
    acknowledge: (id, token) => processControl('recording-acknowledge', { id, token }),
    stop: id => processControl('recording-stop', { id }),
    clear: id => processControl('recording-clear', { id })
  }
})
