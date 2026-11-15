'use strict'

const { capacity, opaqueId, resourceCount, version, versionRange } = require('../../src/backend-contract/primitives')
const {
  attachBleBackend,
  BleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const {
  deterministicScenarioAdvertisement,
  managerScenarioScanOptions
} = require('../../src/testing/scenarios/manager-scenario-executor')
const { driveVirtualClock } = require('../helpers/async')

const compatibility = {
  backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
  capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
  eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
  traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
}

function delivery() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(128),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
}

function subscribeOptions(deliveryMode) {
  return { signal: null, deadline: null, delivery: delivery(), deliveryMode }
}

async function openManager() {
  const fixture = createDeterministicTestBackend()
  const modes = []
  const original = fixture.backend.gatt.subscribe
  fixture.backend.gatt.subscribe = (path, request) => {
    modes.push(request.options.deliveryMode)
    return original(path, request)
  }
  const attached = await attachBleBackend(fixture.backend, compatibility)
  const manager = await BleManager.create(
    {
      attachedBackend: attached,
      clientId: opaqueId('delivery-client', 'client', 'delivery-key'),
      managerId: opaqueId('delivery-manager', 'manager', 'delivery-key'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  return { fixture, manager, modes }
}

async function dualPropertyCharacteristic(fixture, manager) {
  const scan = await driveVirtualClock(
    fixture.controller.clock,
    manager.scan(managerScenarioScanOptions(4, 128)),
    'scan'
  )
  const pending = scan.observations[Symbol.asyncIterator]().next()
  fixture.controller.emitAdvertisement(deterministicScenarioAdvertisement())
  const received = await driveVirtualClock(fixture.controller.clock, pending, 'advertisement')
  if (received.done || received.value.kind !== 'value') {
    throw new Error('scan did not observe the deterministic peer')
  }
  const connection = await driveVirtualClock(
    fixture.controller.clock,
    manager.connect(received.value.value.device.id, { signal: null, deadline: null }),
    'connect'
  )
  const database = await driveVirtualClock(
    fixture.controller.clock,
    connection.discover({ signal: null, deadline: null }),
    'discover'
  )
  const characteristic = (await database.snapshot()).characteristics.find(
    entry => entry.properties.notify === true && entry.properties.indicate === true
  )
  if (characteristic === undefined) {
    throw new Error('deterministic peripheral has no notify+indicate characteristic')
  }
  await driveVirtualClock(fixture.controller.clock, scan.stop(), 'stop scan')
  return { database, path: characteristic.path }
}

test('identical require-notification subscriptions share one physical enable', async () => {
  const { fixture, manager, modes } = await openManager()
  const { database, path } = await dualPropertyCharacteristic(fixture, manager)
  const first = await driveVirtualClock(
    fixture.controller.clock,
    database.subscribe(path, subscribeOptions('require-notification')),
    'first require-notification'
  )
  const second = await driveVirtualClock(
    fixture.controller.clock,
    database.subscribe(path, subscribeOptions('require-notification')),
    'second require-notification'
  )
  expect(modes).toEqual(['require-notification'])
  await driveVirtualClock(fixture.controller.clock, second.remove(), 'remove second')
  await driveVirtualClock(fixture.controller.clock, first.remove(), 'remove first')
  await driveVirtualClock(fixture.controller.clock, manager.destroy(), 'destroy')
})

test('require-indication does not join a prefer-indication physical enable', async () => {
  const { fixture, manager, modes } = await openManager()
  const { database, path } = await dualPropertyCharacteristic(fixture, manager)
  const first = await driveVirtualClock(
    fixture.controller.clock,
    database.subscribe(path, subscribeOptions('prefer-indication')),
    'prefer-indication'
  )
  // The deterministic backend enables indication for both modes and returns
  // the first enable's terminal. The registry must still dispatch the hard
  // request, then refuse that foreign terminal without dropping the first
  // physical enable. Desktop radios refuse the same join as capability.limited
  // before a second CCCD write.
  await expect(
    driveVirtualClock(
      fixture.controller.clock,
      database.subscribe(path, subscribeOptions('require-indication')),
      'require-indication'
    )
  ).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
  expect(modes).toEqual(['prefer-indication', 'require-indication'])
  expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
  await driveVirtualClock(fixture.controller.clock, first.remove(), 'remove prefer')
  expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
  await driveVirtualClock(fixture.controller.clock, manager.destroy(), 'destroy')
})

test.each(['remove', 'destroy'])(
  'subscription %s releases source-loss payload before quota is unregistered',
  async action => {
    const { fixture, manager } = await openManager()
    const { database, path } = await dualPropertyCharacteristic(fixture, manager)
    const options = {
      ...subscribeOptions('require-notification'),
      delivery: { ...delivery(), overflowPolicy: 'error' }
    }
    const subscription = await driveVirtualClock(
      fixture.controller.clock,
      database.subscribe(path, options),
      'subscribe'
    )
    const stream = subscription.values
    stream.emit({ value: new Uint8Array([1, 2, 3]), delivery: 'notification' }, 3)
    stream.observeSourceLoss({
      kind: 'source-loss',
      scope: 'host-global',
      source: 'notification-ingress',
      lostRecords: resourceCount(1)
    })
    expect(stream.sourceLossNotices()).toMatchObject([{ kind: 'source-loss', lostRecords: 1 }])
    expect(stream.retainedPayloadBytes()).toBe(3)
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(3)
    await driveVirtualClock(
      fixture.controller.clock,
      action === 'remove' ? subscription.remove() : manager.destroy(),
      action
    )
    expect(stream.retainedPayloadBytes()).toBe(0)
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(0)
    const iterator = stream[Symbol.asyncIterator]()
    // Source loss is retained on the explicit provenance projection. The
    // stable iterator exposes its normal terminal after cleanup.
    expect((await iterator.next()).value).toMatchObject({ kind: 'terminal' })
    expect((await iterator.next()).done).toBe(true)
    if (action === 'remove') await driveVirtualClock(fixture.controller.clock, manager.destroy(), 'destroy')
  }
)
