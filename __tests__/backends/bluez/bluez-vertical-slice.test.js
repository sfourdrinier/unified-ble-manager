// __tests__/backends/bluez/bluez-vertical-slice.test.js

const { attachBackend } = require('../../../src/backend-contract/backend')
const { capacity, opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../../src/manager/ble-manager')
const { findTckScenario } = require('../../../src/tck')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
  BLUEZ_GATT_DESCRIPTOR_INTERFACE,
  BLUEZ_GATT_SERVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'
const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const descriptorUuid = '00002902-0000-1000-8000-00805f9b34fb'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function delivery(itemCapacity = 4, overflowPolicy = 'drop-oldest') {
  return {
    itemCapacity: capacity(itemCapacity),
    byteCapacity: capacity(4096),
    reservedControlCapacity: capacity(1),
    overflowPolicy
  }
}

function operation(signal = null) {
  return { signal, deadline: null }
}

function deferred() {
  let resolve = null
  let reject = null
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

async function flushMicrotasks() {
  for (let ordinal = 0; ordinal < 16; ordinal += 1) {
    await Promise.resolve()
  }
}

function scanOptions() {
  return {
    filter: { serviceUuids: [serviceUuid], manufacturerData: [], localNamePrefix: 'Polar' },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: true }
  }
}

function managedObjects() {
  const service0 = `${devicePath}/service0001`
  const service1 = `${devicePath}/service0002`
  const characteristic0 = `${service0}/char0001`
  const characteristic1 = `${service1}/char0001`
  return [
    {
      path: adapterPath,
      interfaces: [
        {
          name: BLUEZ_ADAPTER_INTERFACE,
          properties: {
            Address: { signature: 's', value: '00:11:22:33:44:55' },
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true },
            Discovering: { signature: 'b', value: false }
          }
        }
      ]
    },
    {
      path: devicePath,
      interfaces: [
        {
          name: BLUEZ_DEVICE_INTERFACE,
          properties: {
            Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
            AddressType: { signature: 's', value: 'random' },
            Alias: { signature: 's', value: 'Polar H10' },
            RSSI: { signature: 'n', value: -48 },
            UUIDs: { signature: 'as', value: [serviceUuid] },
            Connected: { signature: 'b', value: true },
            ServicesResolved: { signature: 'b', value: true }
          }
        }
      ]
    },
    serviceObject(service0),
    serviceObject(service1),
    characteristicObject(characteristic0, service0),
    characteristicObject(characteristic1, service1),
    {
      path: `${characteristic0}/desc0001`,
      interfaces: [
        {
          name: BLUEZ_GATT_DESCRIPTOR_INTERFACE,
          properties: {
            Characteristic: { signature: 'o', value: characteristic0 },
            UUID: { signature: 's', value: descriptorUuid }
          }
        }
      ]
    }
  ]
}

function serviceObject(path) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_SERVICE_INTERFACE,
        properties: {
          Device: { signature: 'o', value: devicePath },
          UUID: { signature: 's', value: serviceUuid },
          Primary: { signature: 'b', value: true }
        }
      }
    ]
  }
}

function characteristicObject(path, service) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        properties: {
          Service: { signature: 'o', value: service },
          UUID: { signature: 's', value: characteristicUuid },
          Flags: { signature: 'as', value: ['read', 'write', 'notify'] },
          Value: { signature: 'ay', value: new Uint8Array([1]) },
          Notifying: { signature: 'b', value: false }
        }
      }
    ]
  }
}

async function backendFixture(now = () => 20) {
  const boundary = new InMemoryBluezBoundary({ objects: managedObjects() })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  await attachBackend(backend, compatibility())
  return { backend, boundary }
}

async function connectedDatabase(backend) {
  const peerId = await observedPeerId(backend)
  const clientId = opaqueId('client-1', 'client', 'bluez:test')
  const lease = await backend.connections.connect(peerId, clientId, operation())
  const database = await backend.gatt.discover(lease.connection, operation())
  return { lease, database }
}

async function observedPeerId(backend) {
  const scan = await backend.scanner.start(scanOptions(), opaqueId('peer-observer', 'client', 'bluez:test'))
  const observation = await scan.observations[Symbol.asyncIterator]().next()
  await scan.stop()
  if (observation.done || observation.value.kind !== 'value') {
    throw new Error('BlueZ test fixture did not emit a peer observation')
  }
  return observation.value.value.device.id
}

async function managerFixture() {
  const boundary = new InMemoryBluezBoundary({ objects: managedObjects() })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 20
  })
  const manager = await createBleManagerFromProvider(
    {
      provider,
      selection: { selectedAdapterId: adapterPath },
      coreCompatibility: compatibility(),
      manager: {
        clientId: opaqueId('bluez-manager-client', 'client', 'bluez:manager'),
        managerId: opaqueId('bluez-manager', 'manager', 'bluez:manager'),
        ownerMode: 'owning'
      }
    },
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  return { manager, boundary }
}

describe('BlueZ contract-v1 vertical slice', () => {
  test('binds the applicable continuous-scan production TCK facts to the BlueZ mock boundary', async () => {
    const definition = findTckScenario('scan.fairness-abort-deadline-and-final-cleanup')
    expect(definition.requiredFacts).toEqual([
      'scan-consumer-release-is-fair-and-isolated',
      'scan-abort-and-deadline-close-ingress',
      'scan-stop-resolves-before-final-physical-release',
      'scan-no-late-observation-after-stop'
    ])
    const { backend, boundary } = await backendFixture()
    const abortController = new AbortController()
    const owner = await backend.scanner.start(
      { ...scanOptions(), signal: abortController.signal },
      opaqueId('tck-owner', 'client', 'bluez:tck')
    )
    const ownerIterator = owner.observations[Symbol.asyncIterator]()
    await ownerIterator.next()
    const joined = await backend.scanner.join(
      owner.leaseId,
      owner.shareToken,
      opaqueId('tck-joined', 'client', 'bluez:tck')
    )
    await joined.stop()
    expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(0)

    let releasePhysicalStop
    const physicalStop = new Promise(resolve => {
      releasePhysicalStop = resolve
    })
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => physicalStop)
    abortController.abort()
    let stopSettled = false
    const ownerStop = owner.stop().then(result => {
      stopSettled = true
      return result
    })
    await Promise.resolve()
    expect(stopSettled).toBe(false)
    releasePhysicalStop()
    await expect(ownerStop).resolves.toEqual({ state: 'released', failures: [] })

    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      RSSI: { signature: 'n', value: -30 }
    })
    await expect(ownerIterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
    await expect(
      backend.scanner.start({ ...scanOptions(), deadline: 20 }, opaqueId('tck-deadline', 'client', 'bluez:tck'))
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('runs the public manager scan-connect-discover-read-write-notify-destroy journey', async () => {
    const { manager, boundary } = await managerFixture()
    const scan = await manager.scan(scanOptions())
    const scanIterator = scan.observations[Symbol.asyncIterator]()
    const observation = await scanIterator.next()
    expect(observation).toMatchObject({
      done: false,
      value: { kind: 'value', value: { localName: { state: 'present', value: 'Polar H10' } } }
    })
    await scan.stop()

    const connection = await manager.connect(observation.value.value.device.id, operation())
    const database = await connection.discover(operation())
    const snapshot = await database.snapshot()
    const characteristic = snapshot.characteristics[0].path
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'ReadValue',
      async () => new Uint8Array([12, 13])
    )
    await expect(database.read(characteristic, operation())).resolves.toEqual(new Uint8Array([12, 13]))
    await expect(
      database.write(characteristic, new Uint8Array([14]), { ...operation(), mode: 'with-response' })
    ).resolves.toMatchObject({ commitState: 'confirmed', terminal: { outcome: 'succeeded' } })

    const subscription = await database.subscribe(characteristic, {
      ...operation(),
      delivery: delivery()
    })
    const notification = subscription.values[Symbol.asyncIterator]().next()
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Value: { signature: 'ay', value: new Uint8Array([15]) } }
    )
    await expect(notification).resolves.toMatchObject({
      done: false,
      value: { kind: 'value', value: { value: new Uint8Array([15]) } }
    })

    await expect(manager.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(Object.values(manager.localResourceCounters()).every(value => Number(value) === 0)).toBe(true)
    expect(boundary.objectManager.listenerCount()).toBe(0)
    expect(boundary.closed).toBe(true)
  })

  test('composes the physical discovery filter, enforces one owner, and emits copied observations', async () => {
    const { backend, boundary } = await backendFixture()
    const clientId = opaqueId('scan-client', 'client', 'bluez:scan')
    const scan = await backend.scanner.start(scanOptions(), clientId)
    const iterator = scan.observations[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: {
        kind: 'value',
        value: {
          localName: { state: 'present', value: 'Polar H10' },
          rssi: { state: 'present', value: -48 }
        }
      }
    })
    expect(boundary.calls.slice(0, 2).map(call => call.method)).toEqual(['SetDiscoveryFilter', 'StartDiscovery'])
    expect(boundary.calls[0].argumentsValue[0]).toMatchObject({
      signature: 'a{sv}',
      value: {
        DuplicateData: { signature: 'b', value: true },
        Pattern: { signature: 's', value: 'Polar' },
        UUIDs: { signature: 'as', value: [serviceUuid] }
      }
    })
    await expect(backend.scanner.start(scanOptions(), clientId)).rejects.toMatchObject({
      normalized: { code: 'scan.already-active' }
    })

    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.calls.slice(-2).map(call => call.method)).toEqual(['StopDiscovery', 'SetDiscoveryFilter'])
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('discovers duplicate UUID occurrences atomically and preserves read/write buffer ownership', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const snapshot = await database.snapshot()
    expect(snapshot.services).toHaveLength(2)
    expect(snapshot.characteristics).toHaveLength(2)
    expect(snapshot.descriptors).toHaveLength(1)
    expect(snapshot.services[0].path.serviceUuid).toBe(snapshot.services[1].path.serviceUuid)
    expect(snapshot.services[0].path.serviceOccurrence).not.toBe(snapshot.services[1].path.serviceOccurrence)
    const characteristic = snapshot.characteristics[0].path
    const source = new Uint8Array([7, 8, 9])
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'ReadValue',
      async () => source
    )

    const read = await database.read(characteristic, operation())
    source[0] = 99
    expect([...read]).toEqual([7, 8, 9])

    let releaseWrite
    const writeGate = new Promise(resolve => {
      releaseWrite = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'WriteValue',
      async () => writeGate
    )
    const writeInput = new Uint8Array([21, 22])
    const writePromise = database.write(characteristic, writeInput, { ...operation(), mode: 'without-response' })
    writeInput[0] = 88
    releaseWrite()
    await writePromise
    const writeCall = boundary.calls.find(call => call.method === 'WriteValue')
    expect([...writeCall.argumentsValue[0].value]).toEqual([21, 22])
    expect(writeCall.argumentsValue[1]).toMatchObject({
      signature: 'a{sv}',
      value: { type: { signature: 's', value: 'command' } }
    })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('orders notification readiness, bounds overflow, and invalidates stale paths on adapter reset', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    let releaseNotify
    const notifyGate = new Promise(resolve => {
      releaseNotify = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => notifyGate
    )
    const subscriptionPromise = database.subscribe(characteristic, {
      ...operation(),
      delivery: delivery(1, 'error')
    })
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Value: { signature: 'ay', value: new Uint8Array([1]) } }
    )
    releaseNotify()
    const subscription = await subscriptionPromise
    const iterator = subscription.values[Symbol.asyncIterator]()
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Value: { signature: 'ay', value: new Uint8Array([2]) } }
    )
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Value: { signature: 'ay', value: new Uint8Array([3]) } }
    )

    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'overflow' }
    })
    await Promise.resolve()
    boundary.objectManager.emitInterfacesRemoved(adapterPath, [BLUEZ_ADAPTER_INTERFACE])
    await expect(database.read(characteristic, operation())).rejects.toMatchObject({
      normalized: { code: 'gatt.stale-handle' }
    })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.objectManager.listenerCount()).toBe(0)
  })

  test('confirms physical notification start and stop before exposing or releasing a subscription', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => false
    )
    let subscribeSettled = false
    const subscribing = database
      .subscribe(characteristic, { ...operation(), delivery: delivery() })
      .then(subscription => {
        subscribeSettled = true
        return subscription
      })
    await Promise.resolve()
    expect(subscribeSettled).toBe(false)
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Notifying: { signature: 'b', value: true } }
    )
    const subscription = await subscribing

    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => false
    )
    const iterator = subscription.values[Symbol.asyncIterator]()
    let removalSettled = false
    const removing = subscription.remove().then(cleanup => {
      removalSettled = true
      return cleanup
    })
    await Promise.resolve()
    expect(removalSettled).toBe(false)
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Value: { signature: 'ay', value: new Uint8Array([18]) } }
    )
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'owner-released' }
    })
    boundary.objectManager.emitPropertiesChanged(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      { Notifying: { signature: 'b', value: false } }
    )
    await expect(removing).resolves.toEqual({ state: 'released', failures: [] })
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    await backend.destroy()
  })

  test('tracks a pending dispatcher unsubscribe before allowing connection disconnect', async () => {
    const { backend, boundary } = await backendFixture()
    const { lease, database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    const subscription = await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
    const stopGate = deferred()
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => {
        await stopGate.promise
      }
    )

    const unsubscribe = backend.gatt.unsubscribe(subscription, {
      ...operation(),
      correlation: opaqueId('unsubscribe', 'core-operation', 'bluez:unsubscribe')
    })
    await flushMicrotasks()
    expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)

    const disconnect = lease.connection.disconnect()
    let disconnectSettled = false
    disconnect.then(() => {
      disconnectSettled = true
    })
    await flushMicrotasks()
    expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(0)
    expect(disconnectSettled).toBe(false)

    stopGate.resolve()
    await expect(unsubscribe.completion).resolves.toMatchObject({ outcome: 'succeeded' })
    await expect(disconnect).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
    await backend.destroy()
  })

  test('destroys an unconfirmed physical notification by stopping it after StartNotify returned', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => false
    )
    const subscribing = database.subscribe(characteristic, { ...operation(), delivery: delivery() })
    await new Promise(resolve => setImmediate(resolve))
    const destroying = backend.destroy()
    await expect(subscribing).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
    await expect(destroying).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)
  })

  test('cleans a cancelled notification confirmation on device loss and permits a confirmed reconnect', async () => {
    const { backend, boundary } = await backendFixture()
    const peerId = await observedPeerId(backend)
    const clientId = opaqueId('disconnect-reconnect-client', 'client', 'bluez:disconnect-reconnect')
    const lease = await backend.connections.connect(peerId, clientId, operation())
    const database = await backend.gatt.discover(lease.connection, operation())
    const characteristic = (await database.snapshot()).characteristics[0].path
    let releaseStartNotify
    const startNotifyGate = new Promise(resolve => {
      releaseStartNotify = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => {
        await startNotifyGate
        return false
      }
    )
    const abortController = new AbortController()
    const cancelledSubscription = database.subscribe(characteristic, {
      ...operation(abortController.signal),
      delivery: delivery()
    })
    await Promise.resolve()
    abortController.abort()
    await expect(cancelledSubscription).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    releaseStartNotify()
    await new Promise(resolve => setImmediate(resolve))
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)

    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    const reconnectableLease = await backend.connections.connect(peerId, clientId, operation())
    const reconnectableDatabase = await backend.gatt.discover(reconnectableLease.connection, operation())
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => false)
    let disconnectSettled = false
    const disconnecting = reconnectableLease.connection.disconnect().then(cleanup => {
      disconnectSettled = true
      return cleanup
    })
    await Promise.resolve()
    expect(disconnectSettled).toBe(false)
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    await expect(disconnecting).resolves.toEqual({ state: 'released', failures: [] })
    await expect(reconnectableDatabase.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })

    const reconnectedLease = await backend.connections.connect(peerId, clientId, operation())
    const reconnectedDatabase = await backend.gatt.discover(reconnectedLease.connection, operation())
    const reconnectedSnapshot = await reconnectedDatabase.snapshot()
    expect(reconnectedSnapshot.characteristics.some(item => item.path.characteristicUuid === characteristicUuid)).toBe(
      true
    )
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => undefined)
    await expect(reconnectedLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await backend.destroy()
  })

  test('cancels a dispatched read and rejects its late D-Bus result without contaminating a later read', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    let releaseRead
    const readGate = new Promise(resolve => {
      releaseRead = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'ReadValue',
      async () => readGate
    )
    const abortController = new AbortController()
    const cancelled = database.read(characteristic, operation(abortController.signal))
    abortController.abort()
    await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    releaseRead(new Uint8Array([40]))
    await Promise.resolve()
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'ReadValue',
      async () => new Uint8Array([41])
    )
    await expect(database.read(characteristic, operation())).resolves.toEqual(new Uint8Array([41]))
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('retains the physical connection when core quarantine times out before native GATT settlement', async () => {
    jest.useFakeTimers()
    let manager = null
    let release = null
    const readGate = deferred()
    try {
      const fixture = await managerFixture()
      manager = fixture.manager
      const { boundary } = fixture
      const backend = manager.attachedBackend.backend
      const peerId = await observedPeerId(backend)
      const connection = await manager.connect(peerId, operation())
      const database = await connection.discover(operation())
      const characteristic = (await database.snapshot()).characteristics[0].path
      boundary.onCall(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        'ReadValue',
        async () => readGate.promise
      )

      const read = database.read(characteristic, operation())
      await flushMicrotasks()
      expect(backend.resourceCounters()).toMatchObject({ dispatchedOperations: 1 })

      release = connection.release()
      await expect(read).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
      jest.runOnlyPendingTimers()
      await flushMicrotasks()

      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(0)
      let releaseResult = null
      release.then(result => {
        releaseResult = result
      })
      await flushMicrotasks()
      expect(releaseResult).toBeNull()
      jest.runOnlyPendingTimers()
      await flushMicrotasks()
      expect(releaseResult).toMatchObject({
        state: 'release-failed',
        failures: expect.arrayContaining([
          expect.objectContaining({
            resourceKind: 'operation-quarantine',
            error: expect.objectContaining({ code: 'operation.timed-out', domain: 'cleanup' })
          }),
          expect.objectContaining({
            resourceKind: 'connection',
            error: expect.objectContaining({ code: 'operation.timed-out', domain: 'cleanup' })
          })
        ])
      })
      const device = boundary.objectManager.objects.find(object => object.path === devicePath)
      expect(device.interfaces[0].properties.Connected.value).toBe(true)
      expect(backend.resourceCounters()).toMatchObject({
        physicalLinks: 1,
        connectionLeases: 1,
        dispatchedOperations: 1
      })

      readGate.resolve(new Uint8Array([4, 2]))
      await flushMicrotasks()
      await expect(connection.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
      expect(backend.resourceCounters()).toMatchObject({
        physicalLinks: 0,
        connectionLeases: 0,
        dispatchedOperations: 0
      })
    } finally {
      readGate.resolve(new Uint8Array([4, 2]))
      if (release !== null) await release.catch(() => undefined)
      if (manager !== null) await manager.destroy()
      jest.useRealTimers()
    }
  })

  test('cleans a failed connect record and rejects a ServicesResolved wait on disconnect', async () => {
    const { backend, boundary } = await backendFixture()
    const peerId = await observedPeerId(backend)
    const clientId = opaqueId('failure-client', 'client', 'bluez:failure')
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      throw new Error('connect failed')
    })
    await expect(backend.connections.connect(peerId, clientId, operation())).rejects.toMatchObject({
      normalized: { code: 'platform.failure' }
    })
    expectConsoleErrorMatching(
      '[connectBluezConnection] Shared BlueZ connect transition failed:',
      expect.objectContaining({ normalized: expect.objectContaining({ operation: 'bluez.connect' }) })
    )
    expect(Number(backend.resourceCounters().physicalLinks)).toBe(0)
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(0)
    boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Connect', async () => {
      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: true }
      })
    })
    const lease = await backend.connections.connect(peerId, clientId, operation())
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      ServicesResolved: { signature: 'b', value: false }
    })
    const discovery = backend.gatt.discover(lease.connection, operation())
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    await expect(discovery).rejects.toMatchObject({
      normalized: { code: 'operation.disconnected' }
    })
    expect(Number(backend.resourceCounters().physicalLinks)).toBe(0)
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('shares one physical connect while cancelling only a joiner and issues no early lease', async () => {
    const { backend, boundary } = await backendFixture()
    const peerId = await observedPeerId(backend)
    const clientId = opaqueId('shared-connect-client', 'client', 'bluez:connect-race')
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    const owner = backend.connections.connect(peerId, clientId, operation())
    const abortController = new AbortController()
    const joiner = backend.connections.connect(peerId, clientId, operation(abortController.signal))
    abortController.abort()

    await expect(joiner).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(boundary.calls.filter(call => call.method === 'Connect')).toHaveLength(1)
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(0)
    expect(Number(backend.resourceCounters().physicalLinks)).toBe(0)
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: true }
    })
    const lease = await owner
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(1)
    expect(Number(backend.resourceCounters().physicalLinks)).toBe(1)
    const retainedLease = await backend.connections.connect(peerId, clientId, operation())
    const ownerDatabase = await backend.gatt.discover(lease.connection, operation())
    await lease.release()
    await expect(ownerDatabase.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
    await retainedLease.release()

    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    const disconnectsBefore = boundary.calls.filter(call => call.method === 'Disconnect').length
    const soleAbort = new AbortController()
    const orphaned = backend.connections.connect(peerId, clientId, operation(soleAbort.signal))
    soleAbort.abort()
    await expect(orphaned).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: true }
    })
    await new Promise(resolve => setImmediate(resolve))
    expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(disconnectsBefore + 1)
    expect(Number(backend.resourceCounters().physicalLinks)).toBe(0)
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('revalidates generation after awaited connect and discovery transitions', async () => {
    const first = await backendFixture()
    const firstPeerId = await observedPeerId(first.backend)
    const resetScan = await first.backend.scanner.start(
      scanOptions(),
      opaqueId('reset-scan-client', 'client', 'bluez:reset')
    )
    const resetScanIterator = resetScan.observations[Symbol.asyncIterator]()
    await resetScanIterator.next()
    first.boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      Connected: { signature: 'b', value: false }
    })
    const connect = first.backend.connections.connect(
      firstPeerId,
      opaqueId('reset-connect-client', 'client', 'bluez:reset'),
      operation()
    )
    first.boundary.objectManager.emitPropertiesChanged(adapterPath, BLUEZ_ADAPTER_INTERFACE, {
      Powered: { signature: 'b', value: false }
    })
    await expect(connect).rejects.toMatchObject({ normalized: { code: 'operation.reset' } })
    expectConsoleErrorMatching(
      '[connectBluezConnection] Shared BlueZ connect transition failed:',
      expect.objectContaining({ normalized: expect.objectContaining({ operation: 'bluez.connect.after-method' }) })
    )
    await expect(resetScanIterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    expect(Number(first.backend.resourceCounters().activeScanControllers)).toBe(0)
    expect(Number(first.backend.resourceCounters().connectionLeases)).toBe(0)
    expect(Number(first.backend.resourceCounters().physicalLinks)).toBe(0)
    await first.backend.destroy()

    const second = await backendFixture()
    const { lease, database } = await connectedDatabase(second.backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    const resetSubscription = await database.subscribe(characteristic, {
      ...operation(),
      delivery: delivery()
    })
    const resetSubscriptionIterator = resetSubscription.values[Symbol.asyncIterator]()
    second.boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      ServicesResolved: { signature: 'b', value: false }
    })
    const discovery = second.backend.gatt.discover(lease.connection, operation())
    second.boundary.emitReset('BlueZ daemon disappeared')
    await expect(discovery).rejects.toMatchObject({ normalized: { code: 'operation.reset' } })
    await expect(resetSubscriptionIterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'connection-lost' }
    })
    expect(Number(second.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(Number(second.backend.resourceCounters().databaseSnapshots)).toBe(0)
    expect(Number(second.backend.resourceCounters().connectionLeases)).toBe(0)
    await second.backend.destroy()
  })

  test('atomically aborts scan startup and retries both physical stop phases without losing ownership', async () => {
    const startup = await backendFixture()
    let releaseFilter
    const filterGate = new Promise(resolve => {
      releaseFilter = resolve
    })
    startup.boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'SetDiscoveryFilter', async call => {
      if (Object.keys(call.argumentsValue[0].value).length > 0) {
        await filterGate
      }
    })
    const abortController = new AbortController()
    const pendingStart = startup.backend.scanner.start(
      { ...scanOptions(), signal: abortController.signal },
      opaqueId('startup-abort-client', 'client', 'bluez:scan-race')
    )
    abortController.abort()
    releaseFilter()
    await expect(pendingStart).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(startup.boundary.calls.filter(call => call.method === 'StartDiscovery')).toHaveLength(0)
    expect(Number(startup.backend.resourceCounters().activeScanControllers)).toBe(0)
    await startup.backend.destroy()

    const stopping = await backendFixture()
    const scan = await stopping.backend.scanner.start(
      scanOptions(),
      opaqueId('stop-retry-client', 'client', 'bluez:scan-race')
    )
    stopping.boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => {
      throw new Error('transient stop failure')
    })
    await expect(scan.stop()).rejects.toThrow('transient stop failure')
    expectConsoleErrorMatching(
      '[stopBluezScan] BlueZ StopDiscovery failed; scan ownership retained for retry:',
      expect.objectContaining({ message: 'transient stop failure' })
    )
    expect(Number(stopping.backend.resourceCounters().activeScanControllers)).toBe(1)
    stopping.boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => undefined)
    await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
    expect(Number(stopping.backend.resourceCounters().activeScanControllers)).toBe(0)
    await stopping.backend.destroy()

    const destroying = await backendFixture()
    let releasePhysicalStart
    const physicalStartGate = new Promise(resolve => {
      releasePhysicalStart = resolve
    })
    destroying.boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StartDiscovery', async () => physicalStartGate)
    const startDuringDestroy = destroying.backend.scanner.start(
      scanOptions(),
      opaqueId('destroy-start-client', 'client', 'bluez:scan-race')
    )
    const destroy = destroying.backend.destroy()
    let destroySettled = false
    destroy.then(() => {
      destroySettled = true
    })
    await Promise.resolve()
    expect(destroySettled).toBe(false)
    releasePhysicalStart()
    await expect(startDuringDestroy).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
    await expect(destroy).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('serializes shared notification enablement and permits exact StopNotify cleanup retry', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    let releaseStart
    const startGate = new Promise(resolve => {
      releaseStart = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => startGate
    )
    const first = database.subscribe(characteristic, { ...operation(), delivery: delivery() })
    const second = database.subscribe(characteristic, { ...operation(), delivery: delivery() })
    expect(boundary.calls.filter(call => call.method === 'StartNotify')).toHaveLength(1)
    releaseStart()
    const firstSubscription = await first
    const secondSubscription = await second
    await firstSubscription.remove()
    expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(0)

    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => {
        throw new Error('transient notify stop failure')
      }
    )
    await expect(secondSubscription.remove()).rejects.toThrow('transient notify stop failure')
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'transient notify stop failure' })
    )
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => undefined
    )
    await expect(secondSubscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)

    let releaseOrphanedStart
    const orphanedStartGate = new Promise(resolve => {
      releaseOrphanedStart = resolve
    })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StartNotify',
      async () => orphanedStartGate
    )
    const abortController = new AbortController()
    const orphanedSubscription = database.subscribe(characteristic, {
      ...operation(abortController.signal),
      delivery: delivery()
    })
    abortController.abort()
    await expect(orphanedSubscription).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    releaseOrphanedStart()
    await new Promise(resolve => setImmediate(resolve))
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    await backend.destroy()
  })

  test('bounds native disconnect confirmation and retries without issuing a second Disconnect', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      const { lease } = await connectedDatabase(backend)
      boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => false)

      let firstResult = null
      const firstDisconnect = lease.connection.disconnect().then(result => {
        firstResult = result
        return result
      })
      await flushMicrotasks()
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await flushMicrotasks()
      expect(firstResult).toMatchObject({
        state: 'release-failed',
        failures: [expect.objectContaining({ resourceKind: 'connection' })]
      })
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)

      boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
        Connected: { signature: 'b', value: false }
      })
      await expect(lease.connection.disconnect()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(firstDisconnect).resolves.toMatchObject({ state: 'release-failed' })
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds an in-flight native Disconnect during destroy and retries it without duplication', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      await connectedDatabase(backend)
      let releaseDisconnect
      const disconnectGate = new Promise(resolve => {
        releaseDisconnect = resolve
      })
      boundary.onCall(devicePath, BLUEZ_DEVICE_INTERFACE, 'Disconnect', async () => disconnectGate)

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({ state: 'release-failed' })
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
      expect(boundary.closed).toBe(false)

      releaseDisconnect()
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(1)
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds an in-flight StopDiscovery during destroy and retries it without duplication', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      await backend.scanner.start(scanOptions(), opaqueId('destroy-stop-discovery', 'client', 'bluez:scan-race'))
      let releaseStop
      const stopGate = new Promise(resolve => {
        releaseStop = resolve
      })
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => stopGate)

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({ state: 'release-failed' })
      expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(1)
      expect(Number(backend.resourceCounters().activeScanControllers)).toBe(1)

      releaseStop()
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.calls.filter(call => call.method === 'StopDiscovery')).toHaveLength(1)
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds an in-flight discovery-filter clear during destroy and retries it without duplication', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      await backend.scanner.start(scanOptions(), opaqueId('destroy-filter-clear', 'client', 'bluez:scan-race'))
      let releaseFilter
      const filterGate = new Promise(resolve => {
        releaseFilter = resolve
      })
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'SetDiscoveryFilter', async call => {
        if (Object.keys(call.argumentsValue[0].value).length === 0) {
          await filterGate
        }
      })

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      expect(
        boundary.calls.filter(
          call => call.method === 'SetDiscoveryFilter' && Object.keys(call.argumentsValue[0].value).length === 0
        )
      ).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({ state: 'release-failed' })
      expect(Number(backend.resourceCounters().activeScanControllers)).toBe(1)

      releaseFilter()
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(
        boundary.calls.filter(
          call => call.method === 'SetDiscoveryFilter' && Object.keys(call.argumentsValue[0].value).length === 0
        )
      ).toHaveLength(1)
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds an in-flight StopNotify during destroy and retries it without duplication', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      const { database } = await connectedDatabase(backend)
      const characteristic = (await database.snapshot()).characteristics[0].path
      await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
      let releaseStopNotify
      const stopNotifyGate = new Promise(resolve => {
        releaseStopNotify = resolve
      })
      boundary.onCall(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        'StopNotify',
        async () => stopNotifyGate
      )

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({ state: 'release-failed' })
      expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)
      expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)

      releaseStopNotify()
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds StopNotify property confirmation and retains ownership for retry', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      const { database } = await connectedDatabase(backend)
      const characteristic = (await database.snapshot()).characteristics[0].path
      await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
      boundary.onCall(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        'StopNotify',
        async () => false
      )

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'subscription',
            error: expect.objectContaining({ code: 'operation.timed-out' })
          }
        ]
      })
      expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)

      boundary.onCall(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        'StopNotify',
        async () => undefined
      )
      boundary.objectManager.emitPropertiesChanged(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        { Notifying: { signature: 'b', value: false } }
      )
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      jest.useRealTimers()
    }
  })

  test('bounds StopDiscovery property confirmation and retains ownership for retry', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      await backend.scanner.start(
        scanOptions(),
        opaqueId('destroy-stop-discovery-confirmation', 'client', 'bluez:scan-race')
      )
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => false)

      const firstDestroy = backend.destroy()
      await flushMicrotasks()
      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstDestroy).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'scan',
            error: expect.objectContaining({ code: 'operation.timed-out' })
          }
        ]
      })
      expect(Number(backend.resourceCounters().activeScanControllers)).toBe(1)

      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => undefined)
      boundary.objectManager.emitPropertiesChanged(adapterPath, BLUEZ_ADAPTER_INTERFACE, {
        Discovering: { signature: 'b', value: false }
      })
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      jest.useRealTimers()
    }
  })

  test('retains child ownership after destroy cleanup failure and permits a full destroy retry', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => {
        throw new Error('transient destroy notify stop failure')
      }
    )
    await database.subscribe(characteristic, { ...operation(), delivery: delivery() })

    const firstDestroy = await backend.destroy()
    expect(firstDestroy).toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'transient destroy notify stop failure' })
    )
    expectConsoleErrorMatching('[BluezBackendRuntime.bluez.destroy.subscription] Cleanup rejected:', expect.anything())
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    expect(boundary.closed).toBe(false)
    expect(boundary.objectManager.listenerCount()).toBeGreaterThan(0)

    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => undefined
    )
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(boundary.closed).toBe(true)
  })

  test('retains logical subscription ownership across destroy StopNotify failure for retryable removal', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    const subscription = await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => {
        throw new Error('persistent destroy notify stop failure')
      }
    )

    await expect(backend.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'persistent destroy notify stop failure' })
    )
    expectConsoleErrorMatching('[BluezBackendRuntime.bluez.destroy.subscription] Cleanup rejected:', expect.anything())
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(1)

    await expect(subscription.remove()).rejects.toThrow('persistent destroy notify stop failure')
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'persistent destroy notify stop failure' })
    )
    await expect(subscription.remove()).rejects.toThrow('persistent destroy notify stop failure')
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'persistent destroy notify stop failure' })
    )
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(1)
  })

  test('retains scan ownership when startup StopDiscovery cleanup fails and retries it during destroy', async () => {
    jest.useFakeTimers()
    try {
      let now = 1_000
      const { backend, boundary } = await backendFixture(() => now)
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StartDiscovery', async () => {
        now = 2_000
        return false
      })
      let stopAttempts = 0
      boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'StopDiscovery', async () => {
        stopAttempts += 1
        if (stopAttempts === 1) {
          throw new Error('transient startup discovery stop failure')
        }
      })

      const starting = backend.scanner.start(
        { ...scanOptions(), deadline: 1_010 },
        opaqueId('startup-stop-retry', 'client', 'bluez:scan-race')
      )
      const startupFailure = expect(starting).rejects.toThrow('BlueZ scan start and cleanup both failed')
      await jest.advanceTimersByTimeAsync(0)
      await startupFailure
      expectConsoleErrorMatching(
        '[startBluezScan] Failed to stop BlueZ discovery after start failure:',
        expect.objectContaining({ message: 'transient startup discovery stop failure' })
      )
      expect(Number(backend.resourceCounters().activeScanControllers)).toBe(1)
      expect(boundary.closed).toBe(false)

      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(stopAttempts).toBe(2)
      expect(Number(backend.resourceCounters().activeScanControllers)).toBe(0)
    } finally {
      jest.useRealTimers()
    }
  })

  test('retains scan ownership when startup filter cleanup fails and retries the filter during destroy', async () => {
    const { backend, boundary } = await backendFixture()
    const abortController = new AbortController()
    let filterClearAttempts = 0
    boundary.onCall(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'SetDiscoveryFilter', async call => {
      if (Object.keys(call.argumentsValue[0].value).length === 0) {
        filterClearAttempts += 1
        if (filterClearAttempts === 1) {
          throw new Error('transient startup filter cleanup failure')
        }
        return
      }
      abortController.abort()
    })

    const starting = backend.scanner.start(
      { ...scanOptions(), signal: abortController.signal },
      opaqueId('startup-filter-retry', 'client', 'bluez:scan-race')
    )
    await expect(starting).rejects.toThrow('BlueZ scan start and cleanup both failed')
    expectConsoleErrorMatching(
      '[startBluezScan] Failed to clear the BlueZ discovery filter after start failure:',
      expect.objectContaining({ message: 'transient startup filter cleanup failure' })
    )
    expect(Number(backend.resourceCounters().activeScanControllers)).toBe(1)
    expect(boundary.closed).toBe(false)

    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(filterClearAttempts).toBe(2)
    expect(Number(backend.resourceCounters().activeScanControllers)).toBe(0)
  })

  test('removes every invalidated database from the connection record', async () => {
    const { backend, boundary } = await backendFixture()
    const { lease, database } = await connectedDatabase(backend)
    expect(Number(backend.resourceCounters().databaseSnapshots)).toBe(1)

    const rediscovered = await backend.gatt.discover(lease.connection, operation())
    await expect(database.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
    expect(Number(backend.resourceCounters().databaseSnapshots)).toBe(1)

    boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
      ServicesResolved: { signature: 'b', value: false }
    })
    await expect(rediscovered.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
    expect(Number(backend.resourceCounters().databaseSnapshots)).toBe(0)
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('cleans subscriptions before releasing a non-final shared connection lease', async () => {
    const { backend, boundary } = await backendFixture()
    const { lease: ownerLease, database } = await connectedDatabase(backend)
    const retainedLease = await backend.connections.connect(
      ownerLease.connection.peerId,
      opaqueId('retained-lease', 'client', 'bluez:shared-lease'),
      operation()
    )
    const characteristic = (await database.snapshot()).characteristics[0].path
    await database.subscribe(characteristic, { ...operation(), delivery: delivery() })

    await expect(ownerLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)
    expect(boundary.calls.filter(call => call.method === 'Disconnect')).toHaveLength(0)
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(1)
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    await expect(database.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })

    await expect(backend.gatt.discover(retainedLease.connection, operation())).resolves.toBeDefined()
    await expect(retainedLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('retains a shared lease subscription cleanup failure for the lease retry', async () => {
    const { backend, boundary } = await backendFixture()
    const { lease: ownerLease, database } = await connectedDatabase(backend)
    const retainedLease = await backend.connections.connect(
      ownerLease.connection.peerId,
      opaqueId('retained-retry-lease', 'client', 'bluez:shared-lease'),
      operation()
    )
    const characteristic = (await database.snapshot()).characteristics[0].path
    let stopAttempts = 0
    boundary.onCall(
      String(characteristic.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => {
        stopAttempts += 1
        if (stopAttempts === 1) {
          throw new Error('transient shared lease notify stop failure')
        }
      }
    )
    await database.subscribe(characteristic, { ...operation(), delivery: delivery() })

    await expect(ownerLease.release()).resolves.toMatchObject({
      state: 'release-failed',
      failures: [{ resourceKind: 'subscription' }]
    })
    expectConsoleErrorMatching(
      '[beginBluezPhysicalRemoval] BlueZ StopNotify failed:',
      expect.objectContaining({ message: 'transient shared lease notify stop failure' })
    )
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(2)
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(1)

    await expect(ownerLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    expect(stopAttempts).toBe(2)
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(1)
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(0)
    await expect(retainedLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('attempts every shared-lease subscription after one cleanup throws', async () => {
    const { backend, boundary } = await backendFixture()
    const { lease: ownerLease, database } = await connectedDatabase(backend)
    const retainedLease = await backend.connections.connect(
      ownerLease.connection.peerId,
      opaqueId('retained-batch-failure-lease', 'client', 'bluez:shared-lease'),
      operation()
    )
    const snapshot = await database.snapshot()
    const first = snapshot.characteristics[0].path
    const second = snapshot.characteristics[1].path
    expect(String(first.ownerLeaseId)).toBe(String(second.ownerLeaseId))
    await database.subscribe(first, { ...operation(), delivery: delivery() })
    await database.subscribe(second, { ...operation(), delivery: delivery() })
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(2)
    const failedPaths = []
    const failStopNotify = async call => {
      failedPaths.push(call.path)
      throw new Error(`shared cleanup failed for ${call.path}`)
    }
    boundary.onCall(
      String(first.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      failStopNotify
    )
    boundary.onCall(
      String(second.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      failStopNotify
    )

    const cleanup = await ownerLease.release()
    expect(cleanup.state).toBe('release-failed')
    expect(failedPaths).toHaveLength(2)
    expect(cleanup.failures).toHaveLength(2)
    expect(cleanup.failures.every(failure => failure.resourceKind === 'subscription')).toBe(true)
    expectConsoleErrorMatching('[beginBluezPhysicalRemoval] BlueZ StopNotify failed:', expect.anything())
    expectConsoleErrorMatching('[beginBluezPhysicalRemoval] BlueZ StopNotify failed:', expect.anything())
    expect(Number(backend.resourceCounters().connectionLeases)).toBe(2)
    expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(2)
    expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(2)

    boundary.onCall(
      String(first.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => undefined
    )
    boundary.onCall(
      String(second.characteristicOccurrence),
      BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
      'StopNotify',
      async () => undefined
    )
    await expect(ownerLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(retainedLease.release()).resolves.toEqual({ state: 'released', failures: [] })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('retains a non-final lease when its subscription cleanup times out', async () => {
    jest.useFakeTimers({ now: 1_000 })
    try {
      const { backend, boundary } = await backendFixture(() => Date.now())
      const { lease: ownerLease, database } = await connectedDatabase(backend)
      const retainedLease = await backend.connections.connect(
        ownerLease.connection.peerId,
        opaqueId('retained-timeout-lease', 'client', 'bluez:shared-lease'),
        operation()
      )
      const characteristic = (await database.snapshot()).characteristics[0].path
      await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
      let releaseStopNotify
      const stopNotifyGate = new Promise(resolve => {
        releaseStopNotify = resolve
      })
      boundary.onCall(
        String(characteristic.characteristicOccurrence),
        BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        'StopNotify',
        async () => stopNotifyGate
      )

      const firstRelease = ownerLease.release()
      await flushMicrotasks()
      expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)

      await jest.advanceTimersByTimeAsync(1_000)
      await expect(firstRelease).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'subscription',
            error: expect.objectContaining({ code: 'operation.timed-out', operation: 'bluez.gatt.stop-notify' })
          }
        ]
      })
      expect(Number(backend.resourceCounters().connectionLeases)).toBe(2)
      expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
      expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(1)

      const repeatedRelease = ownerLease.release()
      await jest.advanceTimersByTimeAsync(1_000)
      await expect(repeatedRelease).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'subscription',
            error: expect.objectContaining({ code: 'operation.timed-out', operation: 'bluez.gatt.stop-notify' })
          }
        ]
      })
      expect(Number(backend.resourceCounters().connectionLeases)).toBe(2)
      expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(1)
      expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(1)

      releaseStopNotify()
      await flushMicrotasks()
      await expect(ownerLease.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.calls.filter(call => call.method === 'StopNotify')).toHaveLength(1)
      expect(Number(backend.resourceCounters().connectionLeases)).toBe(1)
      expect(Number(backend.resourceCounters().physicalCccdEnablements)).toBe(0)
      expect(Number(backend.resourceCounters().subscriptionConsumers)).toBe(0)

      await expect(retainedLease.release()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      jest.useRealTimers()
    }
  })

  test('rejects forged GATT paths and rotates opaque peer handles after object removal', async () => {
    const { backend, boundary } = await backendFixture()
    const scan = await backend.scanner.start(
      scanOptions(),
      opaqueId('opaque-peer-client', 'client', 'bluez:opaque-peer')
    )
    const iterator = scan.observations[Symbol.asyncIterator]()
    const firstObservation = await iterator.next()
    const firstPeerId = firstObservation.value.value.device.id
    expect(String(firstPeerId)).not.toContain('/org/bluez')
    boundary.objectManager.emitInterfacesRemoved(devicePath, [BLUEZ_DEVICE_INTERFACE])
    const device = managedObjects().find(object => object.path === devicePath)
    boundary.objectManager.emitInterfacesAdded(devicePath, device.interfaces)
    const secondObservation = await iterator.next()
    const secondPeerId = secondObservation.value.value.device.id
    expect(secondPeerId).not.toBe(firstPeerId)
    await expect(
      backend.connections.connect(
        firstPeerId,
        opaqueId('stale-peer-client', 'client', 'bluez:opaque-peer'),
        operation()
      )
    ).rejects.toMatchObject({ normalized: { code: 'connection.not-found' } })
    await scan.stop()

    const { database } = await connectedDatabase(backend)
    const characteristic = (await database.snapshot()).characteristics[0].path
    const forged = { ...characteristic, characteristicUuid: descriptorUuid }
    const readsBefore = boundary.calls.filter(call => call.method === 'ReadValue').length
    await expect(database.read(forged, operation())).rejects.toMatchObject({
      normalized: { code: 'gatt.stale-handle' }
    })
    expect(boundary.calls.filter(call => call.method === 'ReadValue')).toHaveLength(readsBefore)
    await backend.destroy()
  })
})
