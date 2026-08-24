// __tests__/advanced-public-manager-facade.test.js

const {
  BleManager: InternalBleManager,
  attachBleBackend,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS,
  createPublicBleManagerFacade
} = require('../src/advanced')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

async function createAdvancedFixture() {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const now = () => Number(fixture.controller.clock.now())
  const internal = await InternalBleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('advanced-facade-client', 'client', 'advanced-facade'),
      managerId: opaqueId('advanced-facade-manager', 'manager', 'advanced-facade'),
      ownerMode: 'owning'
    },
    authority,
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now }
  )
  return { fixture, internal, now }
}

async function settle(fixture, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 20 && !settled; attempt += 1) {
    fixture.controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

test('projects one advanced deterministic manager into the public GATT facade', async () => {
  const { fixture, internal, now } = await createAdvancedFixture()
  const baselineCounters = fixture.backend.resourceCounters()
  const manager = await createPublicBleManagerFacade(internal, now)

  expect(manager.adapter.id).toBe(String(internal.identity.attachment.adapter.adapterId))
  expect(fixture.backend.resourceCounters()).toEqual(baselineCounters)
  expect(manager.diagnostics.resourceCounters()).toEqual(baselineCounters)

  const connection = await settle(fixture, manager.connect('deterministic-peer'))
  expect(connection.peer.id).toBe('deterministic-peer')
  expect(fixture.backend.resourceCounters()).toMatchObject({ connectionLeases: 1, physicalLinks: 1 })
  expect(manager.diagnostics.resourceCounters()).toMatchObject({ connectionLeases: 1, physicalLinks: 1 })

  const database = await settle(fixture, connection.discover())
  const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
  await expect(settle(fixture, characteristic.read())).resolves.toEqual(new Uint8Array([7, 8, 9]))
  await expect(settle(fixture, characteristic.write(new Uint8Array([7, 8])))).resolves.toMatchObject({
    commitState: 'confirmed'
  })
  expect(fixture.controller.peripheral.recordedWrites()).toHaveLength(1)

  const subscription = await settle(fixture, characteristic.subscribe())
  const value = subscription.values[Symbol.asyncIterator]().next()
  fixture.controller.emitNotification(
    {
      serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
      serviceOccurrence: 0,
      characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
      characteristicOccurrence: 0
    },
    new Uint8Array([9])
  )
  await expect(value).resolves.toMatchObject({
    value: { kind: 'value', value: { value: new Uint8Array([9]), delivery: 'notification' } }
  })

  await expect(settle(fixture, subscription.remove())).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(settle(fixture, subscription.remove())).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released', failures: [] })
  expect(fixture.backend.resourceCounters()).toEqual(baselineCounters)
  expect(manager.diagnostics.resourceCounters()).toEqual(baselineCounters)

  await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released', failures: [] })
  await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released', failures: [] })
  expect(fixture.backend.resourceCounters()).toEqual(baselineCounters)
  expect(manager.diagnostics.resourceCounters()).toEqual(baselineCounters)
})
