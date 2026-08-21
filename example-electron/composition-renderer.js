'use strict'

/**
 * Renderer composition. Uses the public BleManager façade and never selects a radio.
 */

const rendererEntrypoint = 'unified-ble-manager/electron/renderer'
const managerExport = 'createElectronRendererBleManager'

async function runRendererJourney(createManager, transport) {
  const manager = await createManager({ transport })
  const scan = await manager.scan()
  await scan.stop()
  await manager.destroy()
}

module.exports = {
  rendererEntrypoint,
  managerExport,
  runRendererJourney
}
