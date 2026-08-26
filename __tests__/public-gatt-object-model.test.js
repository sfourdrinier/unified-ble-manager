// __tests__/public-gatt-object-model.test.js

const {
  attachBleBackend,
  BleManager: InternalBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../src/manager/ble-manager')
const { createPublicBleManager, publicConnectionEvents } = require('../src/public/ble-manager')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')
const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')

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
  test('broadcasts public lifecycle events to independent consumers', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const first = events[Symbol.asyncIterator]()
    const second = events[Symbol.asyncIterator]()
    const firstNext = first.next()
    const secondNext = second.next()
    source.emit(
      {
        kind: 'connection-lifecycle',
        attachment: {},
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        ownerLeaseId: 'lease-1',
        sequence: 1,
        backendIngressOrdinal: null,
        previous: 'connecting',
        current: 'connected',
        cause: 'connected'
      },
      1
    )

    await expect(firstNext).resolves.toMatchObject({ value: { current: 'connected' } })
    await expect(secondNext).resolves.toMatchObject({ value: { current: 'connected' } })
    await first.return()
    await second.return()
  })

  test('closes lifecycle iterators created after the source has terminated', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const first = events[Symbol.asyncIterator]()
    source.closeWithReason('closed')
    await expect(first.next()).resolves.toMatchObject({ done: true })

    const second = events[Symbol.asyncIterator]()
    await expect(second.next()).resolves.toMatchObject({ done: true })
  })

  test('removes lifecycle subscribers when the connection event source terminates', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const iterator = events[Symbol.asyncIterator]()

    source.closeWithReason('closed')
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    expect(events.subscribers.size).toBe(0)

    await iterator.return()
  })

  test('reports failed scan cleanup instead of a false stopped state', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'scan',
              error: {
                code: 'platform.failure',
                domain: 'scan',
                operation: 'public-gatt-object-model.scan-stop',
                platform: null,
                retryability: 'never'
              }
            }
          ]
        })
      })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan()
    const stateIterator = scan.state[Symbol.asyncIterator]()

    await expect(scan.stop()).resolves.toMatchObject({ state: 'release-failed' })
    const states = []
    for (;;) {
      const next = await stateIterator.next()
      if (next.done) break
      states.push(next.value.state)
    }
    expect(states).toEqual(['active', 'stopping', 'failed'])
  })

  test('preserves find abort and timeout causes when scan teardown closes the stream', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let now = 0
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({ observations: source, stop: async () => ({ state: 'released', failures: [] }) })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => now)
    const abortController = new AbortController()
    const aborted = manager.find({ signal: abortController.signal })
    await Promise.resolve()
    abortController.abort()
    source.closeWithReason('owner-released')
    await expect(aborted).rejects.toMatchObject({ code: 'operation.aborted' })

    const timedOut = manager.find({ timeoutMs: 5 })
    await Promise.resolve()
    now = 5
    source.closeWithReason('owner-released')
    await expect(timedOut).rejects.toMatchObject({ code: 'operation.timed-out' })
  })

  test('find keeps selection local and does not forward it as a scan option', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({ observations: source, stop })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const found = manager.find({ select: 'first' })
    source.emit(
      {
        peerId: 'peer-1',
        localName: 'Sensor',
        rssi: -42,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      1
    )

    await expect(found).resolves.toMatchObject({ id: 'peer-1', name: 'Sensor', rssi: -42 })
    expect(internal.scan).toHaveBeenCalledWith(expect.not.objectContaining({ select: 'first' }))
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('find keeps its historical observation budget by default and honours a caller-supplied one', async () => {
    const createFindFixture = () => {
      const source = new CoreBoundedStream(
        { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
        'drop-oldest'
      )
      const internal = {
        identity: null,
        attachedBackend: undefined,
        supports: () => true,
        scan: jest.fn(async () => ({
          observations: source,
          stop: async () => ({ state: 'released', failures: [] })
        })),
        connect: jest.fn(),
        destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      return { source, internal }
    }
    const observe = source => {
      source.emit(
        {
          peerId: 'peer-1',
          localName: 'Sensor',
          rssi: -42,
          serviceUuids: [],
          manufacturerData: [],
          serviceData: []
        },
        1
      )
    }

    const preserved = createFindFixture()
    const defaultManager = await createPublicBleManager(preserved.internal, () => 0)
    const defaultFind = defaultManager.find({ select: 'first' })
    observe(preserved.source)
    await expect(defaultFind).resolves.toMatchObject({ id: 'peer-1' })
    // The historical `delivery: 'latest'` budget: a one-item drop-oldest window.
    expect(preserved.internal.scan).toHaveBeenCalledWith(
      expect.objectContaining({
        delivery: expect.objectContaining({ itemCapacity: 1, byteCapacity: 4 * 1024, overflowPolicy: 'drop-oldest' })
      })
    )

    const widened = createFindFixture()
    const widenedManager = await createPublicBleManager(widened.internal, () => 0)
    const widenedFind = widenedManager.find({ select: 'first', duplicates: 'all', delivery: 'balanced' })
    observe(widened.source)
    await expect(widenedFind).resolves.toMatchObject({ id: 'peer-1' })
    expect(widened.internal.scan).toHaveBeenCalledWith(
      expect.objectContaining({
        delivery: expect.objectContaining({ itemCapacity: 32, byteCapacity: 16 * 1024, overflowPolicy: 'drop-oldest' })
      })
    )
  })

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
