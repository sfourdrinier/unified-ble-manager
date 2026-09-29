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
const { runWithCleanup } = require('../../../src/public/error-bridge')
const { awaitSignal } = require('../../helpers/async')

jest.setTimeout(30000)

test.each(
  ['bluez', 'corebluetooth', 'winrt'].flatMap(platform => [
    [platform, 'release-before-terminal'],
    [platform, 'service-change-before-release'],
    [platform, 'service-change-refused-before-release']
  ])
)('%s public owner retains native cleanup ownership (%s)', async (platform, ordering) => {
  const harness = h.realBinding(platform)
  let reportNativeCleanup
  const nativeCleanup = new Promise(resolve => {
    reportNativeCleanup = resolve
  })
  const openSynthetic = harness.binding.openSynthetic
  harness.binding.openSynthetic = async (owner, options) => {
    const central = await openSynthetic(owner, options)
    return new Proxy(central, {
      get(target, property) {
        if (property === 'unsubscribe')
          return async (...args) => {
            try {
              const value = await target.unsubscribe(...args)
              reportNativeCleanup({ outcome: 'released' })
              return value
            } catch (error) {
              reportNativeCleanup({ outcome: 'refused' })
              throw error
            }
          }
        const value = Reflect.get(target, property)
        return typeof value === 'function' ? (...args) => Reflect.apply(value, target, args) : value
      }
    })
  }
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
  await runWithCleanup(
    async () => {
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
      // Only the invalidation case subscribes before rediscovery. The ordinary
      // owner-release case must not race an autonomous service-change cleanup.
      const characteristicA = databaseA.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
      const serviceChange = ordering !== 'release-before-terminal'
      const refusedInvalidation = ordering === 'service-change-refused-before-release'
      let subscription = serviceChange ? await characteristicA.subscribe() : null
      if (refusedInvalidation) await stage.failNextRadioOp('unsubscribe', 'one invalidated child cleanup refusal')
      const databaseB = await b.discover()
      if (subscription === null) subscription = await characteristicA.subscribe()
      const values = subscription.values[Symbol.asyncIterator]()
      const characteristicB = databaseB.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
      const calls = method => harness.calls.filter(([name]) => name === method)
      expect(calls('connect')).toHaveLength(1)
      if (serviceChange) {
        expect(await h.nextItem(values, 5000)).toMatchObject({ kind: 'terminal', reason: 'service-changed' })
        expect(await awaitSignal(nativeCleanup, 'native cleanup after service change')).toEqual({
          outcome: refusedInvalidation ? 'refused' : 'released'
        })
        expect(calls('unsubscribe')).toHaveLength(1)
        // Join an in-flight refusal or retry one which already settled. In
        // either ordering the exact native obligation must remain retryable.
        const childCleanup = await subscription.remove()
        if (childCleanup.state === 'release-failed') {
          expect(refusedInvalidation).toBe(true)
          await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
        } else {
          expect(childCleanup).toEqual({ state: 'released', failures: [] })
        }
        await expect(a.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
        await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('unsubscribe')).toHaveLength(refusedInvalidation ? 2 : 1)
        if (refusedInvalidation) {
          expect(calls('unsubscribe')[1][1][0]).toMatchObject({
            peerId: calls('unsubscribe')[0][1][0].peerId,
            selector: calls('unsubscribe')[0][1][0].selector,
            consumer: calls('unsubscribe')[0][1][0].consumer
          })
        }
        expect(calls('disconnect')).toHaveLength(0)
        await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
        await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('disconnect')).toHaveLength(1)
        return
      }
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
    },
    async () => {
      const cleanup = await manager.destroy()
      expect(cleanup).toEqual({ state: 'released', failures: [] })
      return cleanup
    }
  )
})
