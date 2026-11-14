'use strict'

// Public logical owners sharing the provider's single native lease. The
// fault and every GATT/release operation execute the actual Rust addon;
// synthetic profiles are deterministic evidence, not physical-radio proof.
const h = require('../../helpers/desktop-rust-core-harness')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { desktopRustCoreError } = require('../../../src/backends/desktop/desktop-rust-core-binding')
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
  let phase = 'lifecycle'
  const nativeOutcomes = []
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
            const call = { phase, request: args[0], outcome: null }
            nativeOutcomes.push(call)
            try {
              const value = await target.unsubscribe(...args)
              call.outcome = { kind: 'released' }
              reportNativeCleanup(call)
              return value
            } catch (error) {
              const normalized = desktopRustCoreError(error, 'desktop-public-shared-release.unsubscribe').normalized
              const cancelled =
                normalized.code === 'operation.aborted' &&
                normalized.domain === 'connection' &&
                normalized.platform?.metadata.coreOperation === 'gatt.unsubscribe'
              const injectedRefusal =
                normalized.code === 'gatt.subscribe-failed' &&
                normalized.domain === 'gatt' &&
                normalized.platform?.metadata.coreOperation === 'gatt.subscribe' &&
                normalized.platform?.safeMessage === 'one invalidated child cleanup refusal'
              call.outcome = {
                kind: cancelled ? 'cancelled' : injectedRefusal ? 'injected-refusal' : 'unexpected-failure',
                error: normalized
              }
              reportNativeCleanup(call)
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
        expect(observedNativeCleanup.outcome.kind).not.toBe('unexpected-failure')
        expect(calls('unsubscribe')).toHaveLength(1)
        const initialCancelled = observedNativeCleanup.outcome.kind === 'cancelled'
        const isInjectedRefusal = error =>
          error.code === 'gatt.subscribe-failed' &&
          error.domain === 'gatt' &&
          error.platform?.metadata.coreOperation === 'gatt.subscribe' &&
          error.platform?.safeMessage === 'one invalidated child cleanup refusal'
        const isJoinedInitialCancellation = error =>
          initialCancelled &&
          ((error.code === 'operation.disconnected' &&
            error.domain === 'core' &&
            error.operation === 'operation-coordinator.cancel-dispatched') ||
            (error.code === observedNativeCleanup.outcome.error?.code &&
              error.domain === observedNativeCleanup.outcome.error?.domain &&
              error.code === 'operation.aborted' &&
              error.domain === 'connection' &&
              error.platform?.metadata.coreOperation === 'gatt.unsubscribe'))
        // An explicit remove can join the cancelled lifecycle cleanup before
        // it settles. Retry only its exact retained obligation, never assume
        // which native dispatch consumed the injected refusal.
        let explicitRemoves = 0
        let childCleanup
        do {
          phase = explicitRemoves === 0 ? 'first-remove' : 'retry-remove'
          childCleanup = await subscription.remove()
          explicitRemoves += 1
          if (childCleanup.state === 'release-failed') {
            expect(childCleanup.failures).toHaveLength(1)
            const failure = childCleanup.failures[0].error
            expect(isInjectedRefusal(failure) || isJoinedInitialCancellation(failure)).toBe(true)
            if (isInjectedRefusal(failure)) expect(refusedInvalidation).toBe(true)
            expect(explicitRemoves).toBeLessThan(3)
          }
        } while (childCleanup.state === 'release-failed')
        expect(childCleanup).toEqual({ state: 'released', failures: [] })
        expect(explicitRemoves).toBeLessThanOrEqual(3)
        expect(nativeOutcomes.every(call => call.outcome.kind !== 'unexpected-failure')).toBe(true)
        expect(nativeOutcomes.filter(call => call.outcome.kind === 'cancelled')).toHaveLength(Number(initialCancelled))
        expect(nativeOutcomes.filter(call => call.outcome.kind === 'injected-refusal')).toHaveLength(
          Number(refusedInvalidation)
        )
        expect(nativeOutcomes.filter(call => call.outcome.kind === 'released')).toHaveLength(1)
        for (const call of nativeOutcomes) {
          expect(call.request).toMatchObject({
            peerId: nativeOutcomes[0].request.peerId,
            selector: nativeOutcomes[0].request.selector,
            consumer: nativeOutcomes[0].request.consumer
          })
        }
        // The indication invalidated B's old graph too. Releasing A retires
        // only A's lease; B recovers its graph on the still-live shared link.
        const unsubscribesAfterChildCleanup = calls('unsubscribe').length
        phase = 'a-release'
        await expect(a.release()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('unsubscribe')).toHaveLength(unsubscribesAfterChildCleanup)
        phase = 'final-remove'
        await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
        expect(calls('unsubscribe')).toHaveLength(unsubscribesAfterChildCleanup)
        const expectedUnsubscribes = 1 + Number(refusedInvalidation) + Number(initialCancelled)
        const actualUnsubscribes = calls('unsubscribe').length
        if (actualUnsubscribes !== expectedUnsubscribes) {
          throw new Error(
            `shared-release unsubscribe trace: ${JSON.stringify({
              expectedUnsubscribes,
              actualUnsubscribes,
              nativeOutcomes
            })}`
          )
        }
        expect(calls('unsubscribe')).toHaveLength(expectedUnsubscribes)
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
