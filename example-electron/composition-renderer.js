'use strict'

/**
 * Renderer composition. Uses ElectronRendererBleClient and never selects a radio.
 */

const rendererEntrypoint = 'unified-ble-manager/electron/renderer'
const clientExport = 'ElectronRendererBleClient'

async function runRendererJourney(client) {
  const scan = await client.scan()
  const connection = await client.connect(scan.peerId)
  const database = await client.discover(connection.handle)
  await client.read(database.handle)
  const subscription = await client.subscribe(database.handle)
  await client.release(subscription.handle)
  await client.destroy()
}

module.exports = {
  rendererEntrypoint,
  clientExport,
  runRendererJourney
}
