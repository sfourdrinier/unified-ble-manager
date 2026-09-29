'use strict'

const { capacity, opaqueId } = require('../../src/backend-contract/primitives')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { driveVirtualClock } = require('../helpers/async')

const operation = () => ({ signal: null, deadline: null })
const correlated = name => ({ ...operation(), correlation: opaqueId(name, 'core-operation', 'cleanup-fixture') })
const options = () => ({
  ...operation(),
  delivery: {
    itemCapacity: capacity(4),
    byteCapacity: capacity(1024),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
})

// Drive the fixture's explicit virtual scheduler, not elapsed time or a
// happened-yet polling verdict. Jest's watchdog only diagnoses nonsettlement.
async function drive(fixture, promise) {
  return driveVirtualClock(fixture.controller.clock, promise, 'deterministic subscription cleanup')
}

async function fixtureWithSubscription() {
  const fixture = createDeterministicTestBackend()
  const peer = opaqueId('cleanup-peer', 'peer', 'cleanup-fixture')
  const lease = await drive(
    fixture,
    fixture.backend.connections.connect(peer, opaqueId('client', 'client', 'cleanup-fixture'), operation())
  )
  const database = await drive(fixture, fixture.backend.gatt.discover(lease.connection, operation()))
  const { path } = (await database.snapshot()).characteristics[0]
  const subscription = await drive(
    fixture,
    fixture.backend.gatt.subscribe(path, { operation: correlated('subscribe'), options: options() }).completion
  )
  // Observe the actual fixture's native cleanup completion, not a fabricated
  // backend outcome or a current-database lookup.
  const physical = [...fixture.backend.physicalSubscriptions.values()][0]
  return { fixture, peer, lease, subscription, physical }
}

test('known original subscription settles after database invalidation and confirmed native retirement', async () => {
  const { fixture, peer, lease, subscription, physical } = await fixtureWithSubscription()
  fixture.controller.triggerServicesChanged(peer)
  expect(await drive(fixture, physical.removePromise)).toEqual({ state: 'released', failures: [] })
  expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
  await expect(
    drive(fixture, fixture.backend.gatt.unsubscribe(subscription, correlated('old-cleanup')).completion)
  ).resolves.toMatchObject({ outcome: 'succeeded', cause: null })
  expect(Number(fixture.backend.resourceCounters().subscriptionConsumers)).toBe(0)
  await expect(
    fixture.backend.gatt.unsubscribe(subscription, correlated('unknown-again')).completion
  ).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
  await drive(fixture, lease.release())
  await drive(fixture, fixture.backend.destroy())
})

test.each(['gatt.stale-handle', 'operation.disconnected'])(
  'invalidated original cleanup retains an actual %s refusal while native scope remains live',
  async failure => {
    const { fixture, peer, lease, subscription, physical } = await fixtureWithSubscription()
    fixture.controller.peripheral.injectFailure('unsubscribe', 'platform.failure')
    fixture.controller.triggerServicesChanged(peer)
    expect((await drive(fixture, physical.removePromise)).state).toBe('release-failed')
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    await expect(
      drive(fixture, fixture.backend.gatt.unsubscribe(subscription, correlated('refused')).completion)
    ).rejects.toMatchObject({ normalized: { code: failure } })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    await expect(
      drive(fixture, fixture.backend.gatt.unsubscribe(subscription, correlated('retry')).completion)
    ).resolves.toMatchObject({ outcome: 'succeeded', cause: null })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    await drive(fixture, lease.release())
    await drive(fixture, fixture.backend.destroy())
  }
)

test('mismatched managed identity is refused without touching its native scope', async () => {
  const { fixture, lease, subscription } = await fixtureWithSubscription()
  const forged = {
    ...subscription,
    path: {
      ...subscription.path,
      connectionGeneration: opaqueId('foreign', 'connection-generation', 'cleanup-fixture')
    }
  }
  await expect(fixture.backend.gatt.unsubscribe(forged, correlated('forged')).completion).rejects.toMatchObject({
    normalized: { code: 'gatt.stale-handle' }
  })
  expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
  await drive(fixture, fixture.backend.gatt.unsubscribe(subscription, correlated('real')).completion)
  await drive(fixture, lease.release())
  await drive(fixture, fixture.backend.destroy())
})
