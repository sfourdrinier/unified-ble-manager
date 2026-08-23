// __tests__/backends/corebluetooth/corebluetooth-lifecycle-quarantine.test.js

const { attachBackend } = require('../../../src/backend-contract/backend')
const { capacity, opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createBleManagerFromProvider, DEFAULT_BLE_MANAGER_OPTIONS } = require('../../../src/manager/ble-manager')
const { createCoreBluetoothBackendProvider } = require('../../../src/backends/corebluetooth/corebluetooth-provider')
const {
  InMemoryCoreBluetoothBoundary
} = require('../../../test-support/corebluetooth/in-memory-corebluetooth-boundary')

const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'

function deferred() {
  let resolve = null
  let reject = null
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function operation(signal = null, deadline = null) {
  return { signal, deadline }
}

function delivery() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(4096),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
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

async function fixture() {
  let boundary = null
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () => {
      boundary = new InMemoryCoreBluetoothBoundary({ serviceUuid, characteristicUuid })
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const backend = await provider.create({
    selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
  })
  await attachBackend(backend, compatibility())
  return { backend, boundary }
}

async function managerFixture() {
  let boundary = null
  const provider = createCoreBluetoothBackendProvider({
    boundaryFactory: () => {
      boundary = new InMemoryCoreBluetoothBoundary({ serviceUuid, characteristicUuid })
      return boundary
    },
    now: () => 20,
    hostKind: 'node'
  })
  const manager = await createBleManagerFromProvider(
    {
      provider,
      selection: {
        selectedAdapterId: opaqueId('corebluetooth-default-adapter', 'adapter', 'corebluetooth')
      },
      coreCompatibility: compatibility(),
      manager: {
        clientId: opaqueId('core-quarantine-client', 'client', 'corebluetooth:core-quarantine'),
        managerId: opaqueId('core-quarantine-manager', 'manager', 'corebluetooth:core-quarantine'),
        ownerMode: 'owning'
      }
    },
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => 20 }
  )
  return { manager, backend: manager.attachedBackend.backend, boundary }
}

async function observedPeerId(backend) {
  const scan = await backend.scanner.start(scanOptions(), opaqueId('observer', 'client', 'corebluetooth:test'))
  backend.boundary.emitAdvertisement()
  const observation = await scan.observations[Symbol.asyncIterator]().next()
  await scan.stop()
  if (observation.done || observation.value.kind !== 'value') {
    throw new Error('CoreBluetooth deterministic boundary did not emit a scan observation')
  }
  return observation.value.value.device.id
}

async function flushMicrotasks() {
  for (let index = 0; index < 12; index += 1) {
    await Promise.resolve()
  }
}

describe('CoreBluetooth late-operation quarantine', () => {
  test('bounds a never-settling stopNotify and retains the original cleanup for retry', async () => {
    jest.useFakeTimers()
    const stopGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      const peerId = await observedPeerId(backend)
      const lease = await backend.connections.connect(
        peerId,
        opaqueId('stop-notify-timeout', 'client', 'corebluetooth:stop-notify-timeout'),
        operation()
      )
      const database = await backend.gatt.discover(lease.connection, operation())
      const characteristic = (await database.snapshot()).characteristics[0].path
      const subscription = await database.subscribe(characteristic, { ...operation(), delivery: delivery() })
      const nativeStopNotify = boundary.stopNotify.bind(boundary)
      let stopNotifyCalls = 0
      boundary.stopNotify = async address => {
        stopNotifyCalls += 1
        await stopGate.promise
        return nativeStopNotify(address)
      }

      const removal = subscription.remove()
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()

      await expect(removal).resolves.toMatchObject({
        state: 'release-failed',
        failures: [{ resourceKind: 'subscription', error: { code: 'operation.timed-out', domain: 'cleanup' } }]
      })
      expect(stopNotifyCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ physicalCccdEnablements: 1 })

      const retryWhilePending = await subscription.remove()
      expect(retryWhilePending).toMatchObject({ state: 'release-failed' })
      expect(stopNotifyCalls).toBe(1)

      stopGate.resolve()
      await flushMicrotasks()
      await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
      expect(stopNotifyCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ physicalCccdEnablements: 0 })
      await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      stopGate.resolve()
      jest.useRealTimers()
    }
  })

  test('bounds a never-settling stopScan and retains the original cleanup for retry', async () => {
    jest.useFakeTimers()
    const stopGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      const scan = await backend.scanner.start(
        scanOptions(),
        opaqueId('stop-scan-timeout', 'client', 'corebluetooth:stop-scan-timeout')
      )
      const nativeStopScan = boundary.stopScan.bind(boundary)
      let stopScanCalls = 0
      boundary.stopScan = async () => {
        stopScanCalls += 1
        await stopGate.promise
        return nativeStopScan()
      }

      const stop = scan.stop()
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()

      await expect(stop).resolves.toMatchObject({
        state: 'release-failed',
        failures: [{ resourceKind: 'scan', error: { code: 'operation.timed-out', domain: 'cleanup' } }]
      })
      expect(stopScanCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ activeScanControllers: 1, scanConsumers: 1 })

      await expect(scan.stop()).resolves.toMatchObject({ state: 'release-failed' })
      expect(stopScanCalls).toBe(1)

      stopGate.resolve()
      await flushMicrotasks()
      await expect(scan.stop()).resolves.toEqual({ state: 'released', failures: [] })
      expect(stopScanCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ activeScanControllers: 0, scanConsumers: 0 })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      stopGate.resolve()
      jest.useRealTimers()
    }
  })

  test('bounds a never-settling disconnect and retains the original cleanup for retry', async () => {
    jest.useFakeTimers()
    const disconnectGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      const peerId = await observedPeerId(backend)
      const lease = await backend.connections.connect(
        peerId,
        opaqueId('disconnect-timeout', 'client', 'corebluetooth:disconnect-timeout'),
        operation()
      )
      const nativeDisconnect = boundary.disconnect.bind(boundary)
      let disconnectCalls = 0
      boundary.disconnect = async nativePeerId => {
        disconnectCalls += 1
        await disconnectGate.promise
        return nativeDisconnect(nativePeerId)
      }

      const release = lease.release()
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()

      await expect(release).resolves.toMatchObject({
        state: 'release-failed',
        failures: [{ resourceKind: 'connection', error: { code: 'operation.timed-out', domain: 'cleanup' } }]
      })
      expect(disconnectCalls).toBe(1)
      expect(boundary.connected).toBe(true)
      expect(backend.resourceCounters()).toMatchObject({ physicalLinks: 1, connectionLeases: 1 })

      await expect(lease.release()).resolves.toMatchObject({ state: 'release-failed' })
      expect(disconnectCalls).toBe(1)

      disconnectGate.resolve()
      await flushMicrotasks()
      await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(disconnectCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ physicalLinks: 0, connectionLeases: 0 })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      disconnectGate.resolve()
      jest.useRealTimers()
    }
  })

  test('memoizes adapter-loss scan cleanup across retries until native stop settles', async () => {
    jest.useFakeTimers()
    const stopGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      await backend.scanner.start(scanOptions(), opaqueId('adapter-loss-scan', 'client', 'corebluetooth:adapter-loss'))
      const nativeStopScan = boundary.stopScan.bind(boundary)
      let stopScanCalls = 0
      boundary.stopScan = async () => {
        stopScanCalls += 1
        await stopGate.promise
        return nativeStopScan()
      }

      boundary.setAdapterState({
        availability: 'unavailable',
        authorization: 'granted',
        power: 'on',
        safeReason: 'lost'
      })
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()
      expect(stopScanCalls).toBe(1)
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      boundary.setAdapterState({
        availability: 'unavailable',
        authorization: 'granted',
        power: 'on',
        safeReason: 'lost'
      })
      await flushMicrotasks()
      expect(stopScanCalls).toBe(1)
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      stopGate.resolve()
      await flushMicrotasks()
      expect(stopScanCalls).toBe(1)
      expect(backend.resourceCounters()).toMatchObject({ activeScanControllers: 0, scanConsumers: 0 })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      stopGate.resolve()
      jest.useRealTimers()
    }
  })

  test('memoizes adapter-loss disconnect cleanup across retries until native disconnect settles', async () => {
    jest.useFakeTimers()
    const disconnectGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      const peerId = await observedPeerId(backend)
      await backend.connections.connect(
        peerId,
        opaqueId('adapter-loss-disconnect', 'client', 'corebluetooth:adapter-loss'),
        operation()
      )
      const nativeDisconnect = boundary.disconnect.bind(boundary)
      let disconnectCalls = 0
      boundary.disconnect = async nativePeerId => {
        disconnectCalls += 1
        await disconnectGate.promise
        return nativeDisconnect(nativePeerId)
      }

      boundary.setAdapterState({
        availability: 'unavailable',
        authorization: 'granted',
        power: 'on',
        safeReason: 'lost'
      })
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()
      expect(disconnectCalls).toBe(1)
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      boundary.setAdapterState({
        availability: 'unavailable',
        authorization: 'granted',
        power: 'on',
        safeReason: 'lost'
      })
      await flushMicrotasks()
      expect(disconnectCalls).toBe(1)
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      disconnectGate.resolve()
      await flushMicrotasks()
      expect(disconnectCalls).toBe(1)
      expect(boundary.connected).toBe(false)
      expect(backend.resourceCounters()).toMatchObject({ physicalLinks: 0, connectionLeases: 0 })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      disconnectGate.resolve()
      jest.useRealTimers()
    }
  })

  test('retries adapter-loss cleanup automatically after native settlement and adapter recovery', async () => {
    jest.useFakeTimers()
    const stopGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      await backend.scanner.start(
        scanOptions(),
        opaqueId('adapter-loss-auto-retry', 'client', 'corebluetooth:adapter-loss')
      )
      const nativeStopScan = boundary.stopScan.bind(boundary)
      boundary.stopScan = async () => {
        await stopGate.promise
        return nativeStopScan()
      }

      boundary.setAdapterState({
        availability: 'unavailable',
        authorization: 'granted',
        power: 'on',
        safeReason: 'lost'
      })
      await flushMicrotasks()
      jest.advanceTimersByTime(1_000)
      await flushMicrotasks()
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      boundary.setAdapterState({ availability: 'available', authorization: 'granted', power: 'on', safeReason: null })
      await flushMicrotasks()
      expectConsoleErrorMatching(
        '[CoreBluetoothBackend.handleAdapterState] Native adapter-loss cleanup requires retry:',
        expect.any(Array)
      )

      stopGate.resolve()
      await flushMicrotasks()
      const replacement = await backend.scanner.start(
        scanOptions(),
        opaqueId('adapter-loss-auto-retry-replacement', 'client', 'corebluetooth:adapter-loss')
      )
      await expect(replacement.stop()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      stopGate.resolve()
      jest.useRealTimers()
    }
  })

  test('retains late notification rollback ownership after pre-enable cleanup failure', async () => {
    const startGate = deferred()
    try {
      const { backend, boundary } = await fixture()
      const peerId = await observedPeerId(backend)
      const lease = await backend.connections.connect(
        peerId,
        opaqueId('late-notify-rollback', 'client', 'corebluetooth:late-notify-rollback'),
        operation()
      )
      const database = await backend.gatt.discover(lease.connection, operation())
      const characteristic = (await database.snapshot()).characteristics[0].path
      const nativeStartNotify = boundary.startNotify.bind(boundary)
      boundary.startNotify = async (address, onValue) => {
        await startGate.promise
        return nativeStartNotify(address, onValue)
      }
      const nativeStopNotify = boundary.stopNotify.bind(boundary)
      let stopNotifyCalls = 0
      boundary.stopNotify = async address => {
        stopNotifyCalls += 1
        if (stopNotifyCalls === 1) {
          return nativeStopNotify(address)
        }
        throw new Error('late notification rollback failed')
      }

      const subscription = backend.gatt.subscribe(characteristic, {
        operation: operation(),
        options: { delivery: delivery() }
      })
      await flushMicrotasks()
      boundary.triggerServicesChanged('native-polar-h10')
      await flushMicrotasks()
      startGate.resolve()
      await expect(subscription.completion).rejects.toBeDefined()
      await flushMicrotasks()
      expectConsoleErrorMatching(
        '[CoreBluetoothGattOperations.subscribe] Cancelled notification cleanup failed:',
        expect.any(Array)
      )

      expect(stopNotifyCalls).toBe(2)
      expect(backend.resourceCounters()).toMatchObject({ physicalCccdEnablements: 1 })

      boundary.stopNotify = nativeStopNotify
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    } finally {
      startGate.resolve()
    }
  })

  test.each(['abort', 'deadline'])(
    'stops a late native notification enable after public %s and releases every subscription resource',
    async termination => {
      if (termination === 'deadline') {
        jest.useFakeTimers()
      }
      try {
        const { backend, boundary } = await fixture()
        const peerId = await observedPeerId(backend)
        const lease = await backend.connections.connect(
          peerId,
          opaqueId(`notify-${termination}`, 'client', 'corebluetooth:notification-quarantine'),
          operation()
        )
        const database = await backend.gatt.discover(lease.connection, operation())
        const characteristic = (await database.snapshot()).characteristics[0].path
        const startGate = deferred()
        const nativeStartNotify = boundary.startNotify.bind(boundary)
        boundary.startNotify = async (address, onValue) => {
          await startGate.promise
          await nativeStartNotify(address, onValue)
        }
        const controller = new AbortController()
        const publicOperation = operation(
          termination === 'abort' ? controller.signal : null,
          termination === 'deadline' ? 21 : null
        )
        const dispatch = backend.gatt.subscribe(characteristic, {
          operation: {
            ...publicOperation,
            correlation: opaqueId(`notify-${termination}`, 'operation', 'corebluetooth:notification-quarantine')
          },
          options: { ...publicOperation, delivery: delivery() }
        })

        await Promise.resolve()
        if (termination === 'abort') {
          controller.abort()
        } else {
          jest.advanceTimersByTime(1)
        }
        await expect(dispatch.completion).rejects.toMatchObject({
          normalized: { code: termination === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
        })

        startGate.resolve()
        await flushMicrotasks()
        await backend.dispatcher.waitForIdle()

        expect(boundary.stopNotifyCalls).toBe(1)
        expect(backend.resourceCounters()).toMatchObject({
          physicalCccdEnablements: 0,
          subscriptionConsumers: 0,
          dispatchedOperations: 0
        })
        await backend.destroy()
      } finally {
        if (termination === 'deadline') {
          jest.useRealTimers()
        }
      }
    }
  )

  test('retains a cancelled pending native connect until late cleanup finishes, preventing duplicate physical ownership', async () => {
    const { backend, boundary } = await fixture()
    const peerId = await observedPeerId(backend)
    const connectGate = deferred()
    const nativeConnect = boundary.connect.bind(boundary)
    const nativeDisconnect = boundary.disconnect.bind(boundary)
    let nativeConnectCalls = 0
    let lateDisconnectFailuresRemaining = 1
    boundary.connect = async nativePeerId => {
      nativeConnectCalls += 1
      await connectGate.promise
      await nativeConnect(nativePeerId)
    }
    boundary.disconnect = async nativePeerId => {
      if (lateDisconnectFailuresRemaining > 0) {
        lateDisconnectFailuresRemaining -= 1
        throw new Error('The deterministic late disconnect cleanup failed')
      }
      await nativeDisconnect(nativePeerId)
    }
    const controller = new AbortController()
    const pending = backend.connections.connect(
      peerId,
      opaqueId('first-client', 'client', 'corebluetooth:connect-quarantine'),
      operation(controller.signal)
    )

    await Promise.resolve()
    controller.abort()
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(
      backend.connections.connect(
        peerId,
        opaqueId('second-client', 'client', 'corebluetooth:connect-quarantine'),
        operation()
      )
    ).rejects.toMatchObject({ normalized: { code: 'connection.already-owned' } })
    expect(nativeConnectCalls).toBe(1)

    connectGate.resolve()
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[CoreBluetoothOperationLifecycle] Late completion cleanup failed:',
      expect.objectContaining({ message: 'The deterministic late disconnect cleanup failed' })
    )
    expect(boundary.connected).toBe(true)
    expect(backend.resourceCounters()).toMatchObject({ physicalLinks: 1, connectionLeases: 0 })

    const lease = await backend.connections.connect(
      peerId,
      opaqueId('third-client', 'client', 'corebluetooth:connect-quarantine'),
      operation()
    )
    expect(nativeConnectCalls).toBe(2)
    await lease.release()
    await backend.destroy()
  })

  test('quarantines direct database read and write calls through the same per-connection dispatcher', async () => {
    const { backend, boundary } = await fixture()
    const peerId = await observedPeerId(backend)
    const lease = await backend.connections.connect(
      peerId,
      opaqueId('direct-gatt-client', 'client', 'corebluetooth:direct-quarantine'),
      operation()
    )
    const database = await backend.gatt.discover(lease.connection, operation())
    const characteristic = (await database.snapshot()).characteristics[0].path
    const readGate = deferred()
    boundary.readGate = readGate.promise
    const controller = new AbortController()
    const read = database.read(characteristic, operation(controller.signal))

    await Promise.resolve()
    controller.abort()
    await expect(read).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(
      database.write(characteristic, new Uint8Array([7]), {
        ...operation(controller.signal),
        mode: 'with-response'
      })
    ).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(boundary.writeValues).toHaveLength(0)

    const destroy = backend.destroy()
    await flushMicrotasks()
    await expect(destroy).resolves.toMatchObject({ state: 'release-failed' })
    expect(boundary.destroyed).toBe(false)
    readGate.resolve(new Uint8Array([6, 6]))
    await flushMicrotasks()
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('retains the physical connection when core quarantine times out before native GATT settlement', async () => {
    jest.useFakeTimers()
    let manager = null
    let readGate = null
    try {
      const fixture = await managerFixture()
      manager = fixture.manager
      const { backend, boundary } = fixture
      const peerId = await observedPeerId(backend)
      const connection = await manager.connect(peerId, operation())
      const database = await connection.discover(operation())
      const characteristic = (await database.snapshot()).characteristics[0].path
      readGate = deferred()
      boundary.readGate = readGate.promise
      const nativeDisconnect = boundary.disconnect.bind(boundary)
      let disconnectCalls = 0
      boundary.disconnect = async nativePeerId => {
        disconnectCalls += 1
        await nativeDisconnect(nativePeerId)
      }

      const read = database.read(characteristic, operation())
      await flushMicrotasks()
      expect(backend.resourceCounters()).toMatchObject({ dispatchedOperations: 1 })

      const release = connection.release()
      await expect(read).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
      jest.runOnlyPendingTimers()
      await flushMicrotasks()

      await expect(release).resolves.toMatchObject({
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
      expect(disconnectCalls).toBe(0)
      expect(boundary.connected).toBe(true)
      expect(backend.resourceCounters()).toMatchObject({
        physicalLinks: 1,
        connectionLeases: 1,
        dispatchedOperations: 1
      })

      readGate.resolve(new Uint8Array([4, 2]))
      await flushMicrotasks()
      await expect(connection.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(disconnectCalls).toBe(1)
      expect(boundary.connected).toBe(false)
      expect(backend.resourceCounters()).toMatchObject({
        physicalLinks: 0,
        connectionLeases: 0,
        dispatchedOperations: 0
      })
    } finally {
      if (readGate !== null) {
        readGate.resolve(new Uint8Array([4, 2]))
      }
      if (manager !== null) {
        await manager.destroy()
      }
      jest.useRealTimers()
    }
  })

  test('returns retryable release failure for pending discovery and releases after settlement', async () => {
    const { backend, boundary } = await fixture()
    const discoverGate = deferred()
    try {
      const peerId = await observedPeerId(backend)
      const lease = await backend.connections.connect(
        peerId,
        opaqueId('pending-discovery-client', 'client', 'corebluetooth:pending-discovery'),
        operation()
      )
      const nativeDiscover = boundary.discover.bind(boundary)
      boundary.discover = async nativePeerId => {
        await discoverGate.promise
        return nativeDiscover(nativePeerId)
      }
      const pendingDiscovery = backend.gatt.discover(lease.connection, operation())
      await Promise.resolve()

      let disconnectCalls = 0
      const nativeDisconnect = boundary.disconnect.bind(boundary)
      boundary.disconnect = async nativePeerId => {
        disconnectCalls += 1
        return nativeDisconnect(nativePeerId)
      }

      await expect(lease.release()).resolves.toMatchObject({
        state: 'release-failed',
        failures: [expect.objectContaining({ resourceKind: 'operation-quarantine' })]
      })
      expect(disconnectCalls).toBe(0)
      expect(boundary.connected).toBe(true)

      discoverGate.resolve()
      await expect(pendingDiscovery).resolves.toMatchObject({ path: expect.any(Object) })
      await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
      expect(disconnectCalls).toBe(1)
      expect(boundary.connected).toBe(false)
    } finally {
      discoverGate.resolve()
      await backend.destroy()
    }
  })

  test('does not disconnect for a permanently blocked discovery and returns promptly', async () => {
    const { backend, boundary } = await fixture()
    const discoverGate = deferred()
    try {
      const peerId = await observedPeerId(backend)
      const lease = await backend.connections.connect(
        peerId,
        opaqueId('blocked-discovery-client', 'client', 'corebluetooth:blocked-discovery'),
        operation()
      )
      boundary.discover = async () => discoverGate.promise
      const pendingDiscovery = backend.gatt.discover(lease.connection, operation())
      await Promise.resolve()
      let disconnectCalls = 0
      boundary.disconnect = async () => {
        disconnectCalls += 1
      }

      const result = await Promise.race([
        lease.release(),
        new Promise(resolve => setTimeout(() => resolve('blocked'), 50))
      ])
      expect(result).not.toBe('blocked')
      expect(result).toMatchObject({ state: 'release-failed' })
      expect(disconnectCalls).toBe(0)
      expect(boundary.connected).toBe(true)

      discoverGate.resolve({ services: [] })
      await expect(pendingDiscovery).resolves.toMatchObject({ path: expect.any(Object) })
    } finally {
      discoverGate.resolve({ services: [] })
      await backend.destroy()
    }
  })

  test('rejects a foreign CoreBluetooth subscription before it can stop the owner boundary notification', async () => {
    const first = await fixture()
    const second = await fixture()
    const peerId = await observedPeerId(first.backend)
    const lease = await first.backend.connections.connect(
      peerId,
      opaqueId('first-owner', 'client', 'corebluetooth:subscription-ownership'),
      operation()
    )
    const database = await first.backend.gatt.discover(lease.connection, operation())
    const characteristic = (await database.snapshot()).characteristics[0].path
    const subscription = await database.subscribe(characteristic, { ...operation(), delivery: delivery() })

    expect(() =>
      second.backend.gatt.unsubscribe(subscription, {
        ...operation(),
        correlation: opaqueId('foreign-unsubscribe', 'operation', 'corebluetooth:subscription-ownership')
      })
    ).toThrow('ownership.denied')
    expect(first.boundary.notificationHandlers.size).toBe(1)
    expect(second.boundary.stopNotifyCalls).toBe(0)

    await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    await first.backend.destroy()
    await second.backend.destroy()
  })

  test.each(['connect', 'discover'])(
    'waits for a native %s operation before destroying the CoreBluetooth boundary',
    async kind => {
      const { backend, boundary } = await fixture()
      const peerId = await observedPeerId(backend)
      const gate = deferred()
      let operationPromise
      if (kind === 'connect') {
        const nativeConnect = boundary.connect.bind(boundary)
        boundary.connect = async nativePeerId => {
          await gate.promise
          await nativeConnect(nativePeerId)
        }
        operationPromise = backend.connections.connect(
          peerId,
          opaqueId('destroy-connect-client', 'client', 'corebluetooth:destroy-quarantine'),
          operation()
        )
      } else {
        const lease = await backend.connections.connect(
          peerId,
          opaqueId('destroy-discover-client', 'client', 'corebluetooth:destroy-quarantine'),
          operation()
        )
        const nativeDiscover = boundary.discover.bind(boundary)
        boundary.discover = async nativePeerId => {
          await gate.promise
          return nativeDiscover(nativePeerId)
        }
        operationPromise = backend.gatt.discover(lease.connection, operation())
      }

      await Promise.resolve()
      const destroy = backend.destroy()
      await flushMicrotasks()
      await expect(destroy).resolves.toMatchObject({ state: 'release-failed' })
      expect(boundary.destroyed).toBe(false)

      gate.resolve()
      if (kind === 'connect') {
        await expect(operationPromise).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
      } else {
        await expect(operationPromise).resolves.toMatchObject({ path: expect.any(Object) })
      }
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(boundary.destroyed).toBe(true)
    }
  )

  test('does not start adapter-loss cleanup after destroy closes admission and retains failed native ownership', async () => {
    const { backend, boundary } = await fixture()
    const peerId = await observedPeerId(backend)
    await backend.connections.connect(
      peerId,
      opaqueId('destroy-adapter-loss-client', 'client', 'corebluetooth:destroy-adapter-loss'),
      operation()
    )
    const disconnectGate = deferred()
    let disconnectCalls = 0
    boundary.disconnect = async () => {
      disconnectCalls += 1
      await disconnectGate.promise
      throw new Error('The native disconnect remained unresolved')
    }

    const destroy = backend.destroy()
    await flushMicrotasks()
    boundary.setAdapterState({ availability: 'unavailable', authorization: 'granted', power: 'on', safeReason: 'lost' })
    await flushMicrotasks()
    const callsBeforeRelease = disconnectCalls

    disconnectGate.reject(new Error('The native disconnect remained unresolved'))
    await expect(destroy).resolves.toMatchObject({
      state: 'release-failed',
      failures: [{ resourceKind: 'connection' }]
    })
    await flushMicrotasks()

    expect(callsBeforeRelease).toBe(1)
    expect(disconnectCalls).toBe(1)
    expect(boundary.destroyed).toBe(false)
    expect(boundary.connected).toBe(true)
    expect(backend.resourceCounters()).toMatchObject({ physicalLinks: 1, connectionLeases: 1 })
  })

  test('keeps a connection readiness watch open until its blocked native disconnect settles', async () => {
    const readinessListeners = new Set()
    const { backend, boundary } = await fixture()
    boundary.canSendWriteWithoutResponse = jest.fn(async nativePeerId => ({
      nativePeerId,
      connectionGeneration: '1',
      ready: true,
      ordinal: 1
    }))
    boundary.onWriteWithoutResponseReadiness = listener => {
      readinessListeners.add(listener)
      return () => readinessListeners.delete(listener)
    }
    const peerId = await observedPeerId(backend)
    const lease = await backend.connections.connect(
      peerId,
      opaqueId('blocked-disconnect-client', 'client', 'corebluetooth:readiness-disconnect'),
      operation()
    )
    const watch = await backend.connectionControls.writeWithoutResponseReadiness(lease.connection)
    const disconnectGate = deferred()
    const nativeDisconnect = boundary.disconnect.bind(boundary)
    boundary.disconnect = async nativePeerId => {
      await disconnectGate.promise
      await nativeDisconnect(nativePeerId)
    }

    const release = lease.release()
    await flushMicrotasks()
    expect(readinessListeners.size).toBe(1)

    disconnectGate.resolve()
    await expect(release).resolves.toEqual({ state: 'released', failures: [] })
    expect(readinessListeners.size).toBe(0)
    await expect(watch.events[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'owner-released' }
    })
    await backend.destroy()
  })
})
