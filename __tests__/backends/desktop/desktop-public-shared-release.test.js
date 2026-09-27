'use strict'

// Public logical owners sharing the provider's single native lease. The
// fault and every GATT/release operation execute the actual Rust addon;
// synthetic profiles are deterministic evidence, not physical-radio proof.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../../src/node-host-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

jest.setTimeout(30000)

test.each(['bluez', 'corebluetooth', 'winrt'])(
  '%s public owner retries refused child cleanup without releasing another owner',
  async platform => {
    const harness = h.realBinding(platform)
    const provider = createTestDesktopRustCoreBackendProvider({
      platform,
      owner: `public-shared-release-${platform}`,
      now: () => performance.now(),
      radio: 'synthetic',
      binding: harness.binding,
      hostPlatform: h.HOST_PLATFORM[platform]
    })
    const internal = await createNodeBleManagerFromProvider(
      provider,
      DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
      { now: () => performance.now() }
    )
    const manager = await createPublicBleManager(internal, () => performance.now())
    const stage = harness.opened.at(-1)
    try {
      await stage.stageServices('peer-1', h.hrmServices())
      const scan = await manager.scan()
      const iterator = scan.observations[Symbol.asyncIterator]()
      const pending = h.nextValue(iterator, 5000)
      await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -50, localName: 'Shared cleanup peer' })
      const observed = await pending
      await iterator.return?.()
      expect(await scan.stop()).toMatchObject({ state: 'released' })
      const a = await manager.connect(observed.peer)
      const b = await manager.connect(observed.peer)
      const databaseA = await a.discover()
      const subscription = await databaseA.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT).subscribe()
      const databaseB = await b.discover()
      const characteristicB = databaseB.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
      const calls = method => harness.calls.filter(([name]) => name === method)
      expect(calls('connect')).toHaveLength(1)
      const unsubscribesBefore = calls('unsubscribe').length
      await stage.failNextRadioOp('unsubscribe', 'one scoped child cleanup refusal')
      const first = await a.disconnect()
      expect(first).toMatchObject({ state: 'release-failed' })
      expect(first.failures.length).toBeGreaterThan(0)
      expect(calls('disconnect')).toHaveLength(0)
      await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
      const unsubscribes = calls('unsubscribe').length
      expect(unsubscribes).toBe(unsubscribesBefore + 1)
      await expect(a.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('unsubscribe')).toHaveLength(unsubscribes + 1)
      const failedChild = calls('unsubscribe')[unsubscribes - 1][1][0]
      expect(calls('unsubscribe')[unsubscribes][1][0]).toMatchObject({
        peerId: failedChild.peerId,
        selector: failedChild.selector,
        consumer: failedChild.consumer
      })
      expect(calls('disconnect')).toHaveLength(0)
      await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('unsubscribe')).toHaveLength(unsubscribes + 1)
      await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
      await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('disconnect')).toHaveLength(1)
      await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('disconnect')).toHaveLength(1)
    } finally {
      expect(await manager.destroy()).toEqual({ state: 'released', failures: [] })
    }
  }
)
