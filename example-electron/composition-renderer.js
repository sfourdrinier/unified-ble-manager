'use strict'

/**
 * Renderer composition. Uses ElectronRendererBleClient and never selects a radio.
 */

const rendererEntrypoint = 'unified-ble-manager/electron/renderer'
const clientExport = 'ElectronRendererBleClient'

async function runRendererJourney(client) {
  await client.initialize()
  await client.request({
    command: 'adapter.state',
    payload: Object.freeze({}),
    binaryPayload: null,
    signal: null
  })
  await client.destroy()
}

module.exports = {
  rendererEntrypoint,
  clientExport,
  runRendererJourney
}
