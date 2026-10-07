'use strict'

const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

const HR = '0000180d-0000-1000-8000-00805f9b34fb'
const BATTERY = '0000180f-0000-1000-8000-00805f9b34fb'

test.each(['corebluetooth', 'winrt', 'bluez'])(
  '%s carries service graph facts through the real addon and public route',
  async platform => {
    const harness = h.realBinding(platform)
    const now = () => performance.now()
    const manager = await createPublicBleManager(
      await createNodeBleManagerFromProvider(
        createTestDesktopRustCoreBackendProvider({
          platform,
          owner: `service-graph-${platform}`,
          now,
          radio: 'synthetic',
          binding: harness.binding,
          hostPlatform: { corebluetooth: 'darwin', winrt: 'win32', bluez: 'linux' }[platform]
        }),
        DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
        { now }
      ),
      now
    )
    const stage = harness.opened.at(-1)
    let scan, connection
    try {
      scan = await manager.scan()
      const observations = scan.observations[Symbol.asyncIterator]()
      const pending = h.nextValue(observations, 5000)
      await stage.stageAdvertisement({ peerId: 'graph-peer', localName: 'Graph' })
      const peer = (await pending).peer
      await observations.return?.()
      await scan.stop()
      connection = await manager.connect(peer)
      // Native source identities need not be the public per-UUID ordinals.
      await stage.stageServices('graph-peer', [
        {
          uuid: HR,
          occurrence: 7,
          primary: true,
          includedServices: [{ uuid: HR, occurrence: 19 }],
          characteristics: []
        },
        { uuid: HR, occurrence: 19, primary: false, includedServices: [], characteristics: [] },
        { uuid: BATTERY, occurrence: 2, primary: null, includedServices: null, characteristics: [] }
      ])
      const database = await connection.discover()
      expect(
        database.services.map(service => ({
          uuid: String(service.uuid),
          occurrence: service.occurrence,
          primary: service.primary,
          includedServices: service.includedServices
        }))
      ).toEqual([
        { uuid: HR, occurrence: 0, primary: true, includedServices: [{ uuid: HR, occurrence: 1 }] },
        { uuid: HR, occurrence: 1, primary: false, includedServices: [] },
        { uuid: BATTERY, occurrence: 0, primary: null, includedServices: null }
      ])
      await stage.stageServicesChanged('graph-peer')
      await stage.stageServices('graph-peer', [
        {
          uuid: HR,
          occurrence: 7,
          primary: true,
          includedServices: [{ uuid: HR, occurrence: 19 }],
          characteristics: []
        }
      ])
      await expect(connection.discover()).rejects.toMatchObject({ code: 'protocol.violation' })
    } finally {
      await connection?.release()
      await scan?.stop()
      await manager.destroy()
    }
  },
  30000
)
