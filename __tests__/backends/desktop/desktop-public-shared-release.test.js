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
  let nativeCleanupError = null
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
              nativeCleanupError = error
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
      // Discover B before the link-level indication. A Services Changed event
      // must invalidate every joined logical owner's old graph, not only A's.
      let databaseB = await b.discover()
      let characteristicB = databaseB.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
      const values = subscription?.values[Symbol.asyncIterator]()
      const bChanges = serviceChange ? databaseB.changed[Symbol.asyncIterator]() : null
      const bChanged = bChanges === null ? null : h.nextValue(bChanges, 5000)
      if (refusedInvalidation) await stage.failNextRadioOp('unsubscribe', 'one invalidated child cleanup refusal')
      // Synthetic radio's explicit production-equivalent Services Changed
      // control, rather than discovery (which only reads the current graph).
      if (serviceChange) await stage.stageServicesChanged('peer-1')
      if (subscription === null) subscription = await characteristicA.subscribe()
      if (serviceChange && values === undefined) throw new Error('service-change subscription was not created')
      const calls = method => harness.calls.filter(([name]) => name === method)
      expect(calls('connect')).toHaveLength(1)
      if (serviceChange) {
        expect(await h.nextItem(values, 5000)).toMatchObject({ kind: 'terminal', reason: 'service-changed' })
        expect(await bChanged).toMatchObject({ reason: 'service-changed' })
        await bChanges.return?.()
        await expect(characteristicB.read()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
        // Services Changed invalidates both logical graphs but is not itself a
        // local disconnect request.
        expect(calls('disconnect')).toHaveLength(0)
        const observedNativeCleanup = await awaitSignal(nativeCleanup, 'native cleanup after service change')
        const expectedNativeCleanup = { outcome: refusedInvalidation ? 'refused' : 'released' }
        if (observedNativeCleanup.outcome !== expectedNativeCleanup.outcome) {
          throw new Error(`unexpected native cleanup outcome: ${observedNativeCleanup.outcome}`, {
            cause: nativeCleanupError
          })
        }
        expect(observedNativeCleanup).toEqual(expectedNativeCleanup)
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
        // The indication invalidated B's old graph too. Releasing A retires
        // only A's lease; B recovers its graph on the still-live shared link.
        await expect(a.release()).resolves.toEqual({ state: 'released', failures: [] })
        await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('unsubscribe')).toHaveLength(refusedInvalidation ? 2 : 1)
        if (refusedInvalidation) {
          expect(calls('unsubscribe')[1][1][0]).toMatchObject({
            peerId: calls('unsubscribe')[0][1][0].peerId,
            selector: calls('unsubscribe')[0][1][0].selector,
            consumer: calls('unsubscribe')[0][1][0].consumer
          })
        }
        expect(calls('disconnect')).toHaveLength(1)
        databaseB = await b.rediscoverGatt({ reason: 'service-changed' })
        expect(calls('connect')).toHaveLength(1)
        characteristicB = databaseB.characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
        await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
        await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('disconnect')).toHaveLength(2)
        return
      }
      const unsubscribesBefore = calls('unsubscribe').length
      await stage.failNextRadioOp('unsubscribe', 'one scoped child cleanup refusal')
      const first = await a.release()
      expect(first).toMatchObject({ state: 'release-failed' })
      expect(first.failures.length).toBeGreaterThan(0)
      // `central.disconnect` is the core lease-release command, not proof of
      // a physical link drop. B remains on the original link (one connect).
      expect(calls('disconnect')).toHaveLength(1)
      await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
      expect(calls('connect')).toHaveLength(1)
      const unsubscribes = calls('unsubscribe').length
      expect(unsubscribes).toBe(unsubscribesBefore + 1)
      await expect(a.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('unsubscribe')).toHaveLength(unsubscribes + 1)
      const failedChild = calls('unsubscribe')[unsubscribes - 1][1][0]
      expect(calls('unsubscribe')[unsubscribes][1][0]).toMatchObject({
        peerId: failedChild.peerId,
        selector: failedChild.selector,
        consumer: failedChild.consumer
      })
      await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('unsubscribe')).toHaveLength(unsubscribes + 1)
      await expect(characteristicB.read()).resolves.toEqual(new Uint8Array([0x42]))
      await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('disconnect')).toHaveLength(2)
      await expect(b.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      expect(calls('disconnect')).toHaveLength(2)
    },
    async () => {
      const cleanup = await manager.destroy()
      expect(cleanup).toEqual({ state: 'released', failures: [] })
      return cleanup
    }
  )
})
