const {
  attachBleBackend,
  BleManager: InternalBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../src/manager/ble-manager')
const { createPublicBleManager } = require('../src/public/ble-manager')
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

async function createPublicFixture() {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const internal = await InternalBleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('public-gatt-client', 'client', 'public-gatt'),
      managerId: opaqueId('public-gatt-manager', 'manager', 'public-gatt'),
      ownerMode: 'owning'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  const manager = await createPublicBleManager(internal, () => Number(fixture.controller.clock.now()))
  return { fixture, manager }
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

async function connectAndDiscover(fixture, manager) {
  const connection = await settle(fixture, manager.connect('deterministic-peer'))
  const database = await settle(fixture, connection.discover())
  return { connection, database }
}

describe('stable public GATT object model (PR3 TDD)', () => {
  test('exposes an immutable generation-bound object graph and explicit duplicate selection', async () => {
    const publicFixture = await createPublicFixture()
    const { fixture, manager } = publicFixture
    const { connection, database } = await connectAndDiscover(fixture, manager)

    expect(database.generation).toEqual(expect.any(String))
    expect(database.services).toHaveLength(2)
    expect(Object.isFrozen(database.services)).toBe(true)
    expect(database.servicesByUuid('180f').map(service => service.occurrence)).toEqual([0, 1])

    expect(() => database.service('180f')).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))
    const secondService = database.service('180f', { occurrence: 1 })
    expect(secondService.occurrence).toBe(1)
    expect(secondService.uuid).toBe('0000180f-0000-1000-8000-00805f9b34fb')
    expect(secondService.characteristics).toHaveLength(2)
    expect(secondService.characteristicsByUuid('2a19').map(value => value.occurrence)).toEqual([0, 1])
    expect(() => secondService.characteristic('2a19')).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))

    const characteristic = secondService.characteristic('2a19', { occurrence: 1 })
    expect(characteristic.properties).toEqual({
      broadcast: false,
      read: true,
      writeWithResponse: true,
      writeWithoutResponse: true,
      authenticatedSignedWrites: false,
      notify: false,
      indicate: false,
      extendedProperties: false,
      reliableWrite: false,
      writableAuxiliaries: false,
      availability: {
        broadcast: 'unknown',
        read: 'known',
        writeWithResponse: 'known',
        writeWithoutResponse: 'known',
        authenticatedSignedWrites: 'unknown',
        notify: 'known',
        indicate: 'known',
        extendedProperties: 'unknown',
        reliableWrite: 'unknown',
        writableAuxiliaries: 'unknown'
      }
    })
    expect(Object.isFrozen(characteristic)).toBe(true)

    await expect(settle(fixture, characteristic.read())).resolves.toEqual(new Uint8Array())
    await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released' })
    await expect(characteristic.read()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })

  test('routes descriptor operations through the captured object path and invalidates the whole graph', async () => {
    const publicFixture = await createPublicFixture()
    const { fixture, manager } = publicFixture
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const descriptor = characteristic.descriptor('2901')
    const changedIterator = database.changed[Symbol.asyncIterator]()
    const subscription = await settle(fixture, characteristic.subscribe())
    const subscriptionIterator = subscription.values[Symbol.asyncIterator]()

    await expect(settle(fixture, descriptor.read())).resolves.toEqual(new Uint8Array([98, 97, 116, 116, 101, 114, 121]))
    await expect(settle(fixture, descriptor.write(new Uint8Array([1, 2])))).resolves.toMatchObject({
      commitState: 'confirmed'
    })

    const preferred = await settle(fixture, characteristic.subscribe({ delivery: 'prefer-indication' }))
    const preferredValue = preferred.values[Symbol.asyncIterator]().next()
    fixture.controller.emitNotification(
      {
        serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
        serviceOccurrence: 0,
        characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: 0
      },
      new Uint8Array([9]),
      true
    )
    await expect(preferredValue).resolves.toMatchObject({
      value: { kind: 'value', value: { delivery: 'indication', value: new Uint8Array([9]) } }
    })
    await settle(fixture, preferred.remove())
    fixture.controller.triggerServicesChanged('deterministic-peer')
    await settle(fixture, Promise.resolve())
    await expect(changedIterator.next()).resolves.toMatchObject({
      done: false,
      value: {
        kind: 'value',
        value: { previousGeneration: database.generation, reason: 'service-changed', affectedHandleRange: null }
      }
    })
    await expect(subscriptionIterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'service-changed' }
    })
    expect(() => database.snapshot()).toThrow(expect.objectContaining({ code: 'gatt.stale-handle' }))
    await expect(descriptor.read()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released' })
    await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })
})
