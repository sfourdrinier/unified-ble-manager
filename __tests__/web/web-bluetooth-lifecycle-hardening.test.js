// __tests__/web/web-bluetooth-lifecycle-hardening.test.js

const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')

const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const DESCRIPTOR = '00002902-0000-1000-8000-00805f9b34fb'
const activeBackends = new Set()

function deferred() {
  let resolve
  let reject
  const promise = new Promise((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

function fixture() {
  const connectDeferred = deferred()
  const startDeferred = deferred()
  const chooserDeferred = deferred()
  const timers = new Set()
  const notificationListeners = new Set()
  const disconnectListeners = new Set()
  let connectMode = 'immediate'
  let chooserMode = 'immediate'
  let startMode = 'immediate'
  let disconnectFailures = 0
  let stopFailures = 0
  let pageListener = null
  const written = []
  const descriptor = {
    uuid: DESCRIPTOR,
    readValue: async () => new Uint8Array([1]),
    writeValue: async value => {
      written.push([...value])
    }
  }
  const characteristic = {
    uuid: CHARACTERISTIC,
    properties: { read: true, write: true, writeWithoutResponse: true, notify: true, indicate: false },
    getDescriptors: async () => [descriptor, { ...descriptor }],
    readValue: async () => new Uint8Array([0, 70]),
    writeValueWithResponse: async value => {
      await Promise.resolve()
      written.push([...value])
    },
    writeValueWithoutResponse: async value => {
      await Promise.resolve()
      written.push([...value])
    },
    startNotifications: async () => {
      if (startMode === 'pending') {
        await startDeferred.promise
      }
      if (startMode === 'overflow') {
        for (const listener of notificationListeners) {
          listener(new Uint8Array([1]))
          listener(new Uint8Array([2]))
        }
      }
    },
    stopNotifications: async () => {
      if (stopFailures > 0) {
        stopFailures -= 1
        throw new Error('stop failed')
      }
    },
    addNotificationListener: listener => notificationListeners.add(listener),
    removeNotificationListener: listener => notificationListeners.delete(listener)
  }
  const service = {
    uuid: SERVICE,
    getCharacteristics: async () => [characteristic, { ...characteristic }]
  }
  const gatt = {
    connected: false,
    connect: async () => {
      if (connectMode === 'pending') {
        await connectDeferred.promise
      }
      gatt.connected = true
    },
    disconnect: () => {
      if (disconnectFailures > 0) {
        disconnectFailures -= 1
        throw new Error('disconnect failed')
      }
      gatt.connected = false
    },
    getPrimaryServices: async () => [service, { ...service }]
  }
  const device = {
    id: 'browser-secret-device',
    gatt,
    addDisconnectListener: listener => disconnectListeners.add(listener),
    removeDisconnectListener: listener => disconnectListeners.delete(listener)
  }
  const selection = { device, grantedServices: [SERVICE] }
  const boundary = {
    implementationVersion: 'hardening-test',
    browserEngine: 'test',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => true,
    requestDevice: async () => {
      if (chooserMode === 'pending') {
        return chooserDeferred.promise
      }
      return selection
    },
    now: () => 10,
    setTimer: callback => {
      const handle = { callback }
      timers.add(handle)
      return handle
    },
    clearTimer: handle => timers.delete(handle),
    addPageLifecycleListener: listener => {
      pageListener = listener
      return () => {
        pageListener = null
      }
    }
  }
  return {
    boundary,
    characteristic,
    chooserDeferred,
    connectDeferred,
    device,
    gatt,
    notificationListeners,
    selection,
    startDeferred,
    timers,
    written,
    disconnectListeners,
    setChooserMode: value => {
      chooserMode = value
    },
    setConnectMode: value => {
      connectMode = value
    },
    setDisconnectFailures: value => {
      disconnectFailures = value
    },
    setStartMode: value => {
      startMode = value
    },
    setStopFailures: value => {
      stopFailures = value
    },
    triggerPage: reason => {
      pageListener(reason)
    }
  }
}

async function backendFixture(testFixture) {
  const provider = createWebBluetoothProvider(testFixture.boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  activeBackends.add(backend)
  return backend
}

async function attachedBoundaryBackend(boundary) {
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  activeBackends.add(backend)
  return backend
}

afterEach(async () => {
  try {
    for (const backend of activeBackends) {
      const cleanup = await backend.destroy()
      if (cleanup.state === 'release-failed') {
        await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      }
    }
  } finally {
    activeBackends.clear()
  }
})

async function selectedPeer(backend) {
  return backend.choose(
    {
      filters: [{ serviceUuids: [SERVICE], manufacturerData: [], localNamePrefix: null }],
      acceptAllDevices: false,
      optionalServices: [SERVICE]
    },
    noDeadline()
  )
}

async function connectedDatabase(backend) {
  const selected = await selectedPeer(backend)
  const lease = await backend.connections.connect(selected.peerId, 'client', noDeadline())
  const database = await backend.gatt.discover(lease.connection, noDeadline())
  return { database, lease, snapshot: await database.snapshot() }
}

async function connectedBoundaryDatabase(boundary) {
  const backend = await attachedBoundaryBackend(boundary)
  const chooser = selectedPeer(backend)
  await boundary.flush()
  boundary.resolveChooser()
  const selected = await chooser
  const lease = await backend.connections.connect(selected.peerId, 'client', noDeadline())
  const database = await backend.gatt.discover(lease.connection, noDeadline())
  return { backend, database, lease, snapshot: await database.snapshot() }
}

function noDeadline() {
  return { signal: null, deadline: null }
}

async function flushMicrotasks() {
  for (let ordinal = 0; ordinal < 8; ordinal += 1) {
    await Promise.resolve()
  }
}

function subscriptionOptions(signal = null, deadline = null, itemCapacity = 2) {
  return {
    signal,
    deadline,
    delivery: {
      itemCapacity,
      byteCapacity: 16,
      reservedControlCapacity: 1,
      overflowPolicy: 'error'
    }
  }
}

const orderedIterators = new WeakMap()
async function orderedRecords(owner, count) {
  let iterator = orderedIterators.get(owner)
  if (iterator === undefined) {
    iterator = owner.stream[Symbol.asyncIterator]()
    orderedIterators.set(owner, iterator)
  }
  const result = []
  for (let index = 0; index < count; index += 1) {
    const item = (await iterator.next()).value
    if (item.kind === 'value') result.push({ kind: item.kind, memberId: item.memberId, ordinal: item.ordinal, value: item.value })
    else if (item.kind === 'loss') result.push({ kind: item.kind, lostValues: item.lostValues, lostBytes: item.lostBytes })
    else result.push(item)
  }
  return result
}

describe('Web Bluetooth lifecycle hardening', () => {
  test('reserves same-peer pending connection ownership before browser connect settles', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    testFixture.setConnectMode('pending')
    const first = backend.connections.connect(selected.peerId, 'first', noDeadline())
    const second = backend.connections.connect(selected.peerId, 'second', noDeadline())

    await expect(second).rejects.toMatchObject({ normalized: { code: 'connection.already-owned' } })
    testFixture.connectDeferred.resolve()
    const lease = await first
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 0, physicalLinks: 0 })
  })

  test('releases the exact connection record without deleting a later same-peer owner', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    const first = await backend.connections.connect(selected.peerId, 'first', noDeadline())
    await first.release()
    const second = await backend.connections.connect(selected.peerId, 'second', noDeadline())

    await first.connection.disconnect()
    expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 1, physicalLinks: 1 })
    await second.release()
    expect(backend.resourceCounters()).toMatchObject({ connectionLeases: 0, physicalLinks: 0 })
  })

  test('clears a failed pending reservation so the same peer can retry', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    testFixture.setConnectMode('pending')
    const first = backend.connections.connect(selected.peerId, 'first', noDeadline())
    testFixture.connectDeferred.reject(new Error('connect failed'))
    await expect(first).rejects.toMatchObject({ normalized: { code: 'connection.failed' } })
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.connect] Browser connect rejected:',
      expect.objectContaining({ message: 'connect failed' })
    )
    testFixture.setConnectMode('immediate')

    const retry = await backend.connections.connect(selected.peerId, 'retry', noDeadline())
    await expect(retry.release()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('clears an aborted pending reservation when the browser connect later rejects', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    testFixture.setConnectMode('pending')
    const controller = new AbortController()
    const first = backend.connections.connect(selected.peerId, 'first', {
      signal: controller.signal,
      deadline: null
    })
    controller.abort()
    await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    testFixture.connectDeferred.reject(new Error('late connect rejection'))
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.connect] Browser connect rejected:',
      expect.objectContaining({ message: 'late connect rejection' })
    )
    testFixture.setConnectMode('immediate')

    const retry = await backend.connections.connect(selected.peerId, 'retry', noDeadline())
    await retry.release()
  })

  test.each(['abort', 'deadline', 'destroy'])('compensates a late browser connect after %s', async termination => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    testFixture.setConnectMode('pending')
    const controller = new AbortController()
    const connect = backend.connections.connect(selected.peerId, 'client', {
      signal: controller.signal,
      deadline: termination === 'deadline' ? 20 : null
    })
    await Promise.resolve()
    if (termination === 'abort') {
      controller.abort()
    } else if (termination === 'deadline') {
      for (const timer of testFixture.timers) {
        timer.callback()
      }
    } else {
      const rejectedConnect = expect(connect).rejects.toMatchObject({
        normalized: { code: 'operation.cancelled-by-destroy' }
      })
      await expect(backend.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      await rejectedConnect
    }
    if (termination !== 'destroy') {
      if (termination === 'deadline') {
        // Finding 161: a dispatched connect whose deadline expires before
        // any link came up is the peer not answering — `connection.failed`
        // (caller-decides) with the deadline fact in `platform`, on every
        // backend. An abort keeps `operation.aborted`.
        await expect(connect).rejects.toMatchObject({
          normalized: {
            code: 'connection.failed',
            domain: 'connection',
            operation: 'web-connection.connect',
            retryability: 'caller-decides',
            platform: { domain: 'web-bluetooth', code: 'DeadlineExpired' }
          }
        })
        const failure = await connect.catch(error => error)
        expect(typeof failure.normalized.platform.metadata.deadlineMs).toBe('number')
        expect(failure.normalized.platform.safeMessage).toMatch(/deadline expired/)
      } else {
        await expect(connect).rejects.toMatchObject({
          normalized: { code: 'operation.aborted' }
        })
      }
    }
    testFixture.connectDeferred.resolve()
    await flushMicrotasks()
    expect(testFixture.gatt.connected).toBe(false)
  })

  test('retains failed late-connect compensation for destroy retry', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    testFixture.setConnectMode('pending')
    testFixture.setDisconnectFailures(1)
    const controller = new AbortController()
    const connect = backend.connections.connect(selected.peerId, 'client', {
      signal: controller.signal,
      deadline: null
    })
    await Promise.resolve()
    controller.abort()
    await expect(connect).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    testFixture.connectDeferred.resolve()
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.compensatePendingConnection] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )

    await expect(backend.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(testFixture.gatt.connected).toBe(false)
  })

  test('shared characteristic keeps physical notifications until the last subscriber releases', async () => {
    const testFixture = fixture()
    const start = jest.spyOn(testFixture.characteristic, 'startNotifications')
    const stop = jest.spyOn(testFixture.characteristic, 'stopNotifications')
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const path = snapshot.characteristics[0].path
    const first = await database.subscribe(path, subscriptionOptions())
    const second = await database.subscribe(path, subscriptionOptions())
    expect(start).toHaveBeenCalledTimes(1)
    expect(testFixture.notificationListeners.size).toBe(1)
    await first.remove()
    expect(stop).not.toHaveBeenCalled()
    const iterator = second.values[Symbol.asyncIterator]()
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([7]))
    expect(await iterator.next()).toMatchObject({ value: { kind: 'value', value: { value: new Uint8Array([7]) } } })
    await second.remove()
    expect(stop).toHaveBeenCalledTimes(1)
    expect(testFixture.notificationListeners.size).toBe(0)
  })

  test('aborting one pending shared start does not stop the other owner', async () => {
    const testFixture = fixture()
    const start = jest.spyOn(testFixture.characteristic, 'startNotifications')
    const stop = jest.spyOn(testFixture.characteristic, 'stopNotifications')
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    testFixture.setStartMode('pending')
    const controller = new AbortController()
    const first = database.subscribe(snapshot.characteristics[0].path, subscriptionOptions(controller.signal))
    const second = database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    await Promise.resolve()
    controller.abort()
    await expect(first).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([9]))
    testFixture.startDeferred.resolve()
    const owner = await second
    await flushMicrotasks()
    expect(start).toHaveBeenCalledTimes(1)
    expect(stop).not.toHaveBeenCalled()
    const iterator = owner.values[Symbol.asyncIterator]()
    expect(await iterator.next()).toMatchObject({ value: { kind: 'value', value: { value: new Uint8Array([9]) } } })
    await owner.remove()
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('joined leases share one characteristic and releasing one retains the other', async () => {
    const testFixture = fixture()
    const start = jest.spyOn(testFixture.characteristic, 'startNotifications')
    const stop = jest.spyOn(testFixture.characteristic, 'stopNotifications')
    const backend = await backendFixture(testFixture)
    const selected = await selectedPeer(backend)
    const firstLease = await backend.connections.connect(selected.peerId, 'first', noDeadline())
    const secondLease = await backend.connections.connect(selected.peerId, 'second', noDeadline())
    const firstDatabase = await backend.gatt.discover(firstLease.connection, noDeadline())
    const secondDatabase = await backend.gatt.discover(secondLease.connection, noDeadline())
    await firstDatabase.subscribe((await firstDatabase.snapshot()).characteristics[0].path, subscriptionOptions())
    const second = await secondDatabase.subscribe((await secondDatabase.snapshot()).characteristics[0].path, subscriptionOptions())
    expect(start).toHaveBeenCalledTimes(1)
    expect(backend.resourceCounters()).toMatchObject({ physicalCccdEnablements: 1, subscriptionConsumers: 2 })
    await firstLease.release()
    expect(stop).not.toHaveBeenCalled()
    const iterator = second.values[Symbol.asyncIterator]()
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([5]))
    expect(await iterator.next()).toMatchObject({ value: { kind: 'value', value: { value: new Uint8Array([5]) } } })
    await second.remove()
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('a new owner waits for an in-flight final stop before starting', async () => {
    const testFixture = fixture()
    const stopDeferred = deferred()
    const stop = jest.spyOn(testFixture.characteristic, 'stopNotifications').mockImplementationOnce(() => stopDeferred.promise)
    const start = jest.spyOn(testFixture.characteristic, 'startNotifications')
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const path = snapshot.characteristics[0].path
    const first = await database.subscribe(path, subscriptionOptions())
    const removing = first.remove()
    const opening = database.subscribe(path, subscriptionOptions())
    await flushMicrotasks()
    expect(start).toHaveBeenCalledTimes(1)
    stopDeferred.resolve()
    await removing
    const second = await opening
    expect(start).toHaveBeenCalledTimes(2)
    await second.remove()
    expect(stop).toHaveBeenCalledTimes(2)
  })

  test('subscription startup overflow never publishes a ready subscription or leaks ownership', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    testFixture.setStartMode('overflow')

    await expect(
      database.subscribe(snapshot.characteristics[0].path, subscriptionOptions(null, null, 1))
    ).rejects.toMatchObject({ normalized: { code: 'stream.overflow' } })
    expect(testFixture.notificationListeners.size).toBe(0)
    expect(backend.resourceCounters()).toMatchObject({
      physicalCccdEnablements: 0,
      subscriptionConsumers: 0
    })
  })

  test('pending notification start abort compensates late success and retries failed stop', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    testFixture.setStartMode('pending')
    testFixture.setStopFailures(1)
    const controller = new AbortController()
    const subscribe = database.subscribe(snapshot.characteristics[0].path, subscriptionOptions(controller.signal))
    await Promise.resolve()
    controller.abort()
    await expect(subscribe).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(testFixture.notificationListeners.size).toBe(0)
    expect(backend.resourceCounters().subscriptionConsumers).toBe(1)
    testFixture.startDeferred.resolve()
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )

    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(testFixture.notificationListeners.size).toBe(0)
  })

  test.each(['deadline', 'destroy'])(
    'pending notification start compensates late success after %s',
    async termination => {
      const testFixture = fixture()
      const backend = await backendFixture(testFixture)
      const { database, snapshot } = await connectedDatabase(backend)
      testFixture.setStartMode('pending')
      const subscribe = database.subscribe(
        snapshot.characteristics[0].path,
        subscriptionOptions(null, termination === 'deadline' ? 20 : null)
      )
      const rejectedSubscription = expect(subscribe).rejects.toMatchObject({
        normalized: {
          code: termination === 'deadline' ? 'operation.timed-out' : 'operation.cancelled-by-destroy'
        }
      })
      await Promise.resolve()
      if (termination === 'deadline') {
        for (const timer of testFixture.timers) {
          timer.callback()
        }
      } else {
        await expect(backend.destroy()).resolves.toMatchObject({ state: 'release-failed' })
      }
      await rejectedSubscription
      expect(testFixture.notificationListeners.size).toBe(0)
      expect(backend.resourceCounters().subscriptionConsumers).toBe(1)
      testFixture.startDeferred.resolve()
      await flushMicrotasks()
      await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
      expect(testFixture.notificationListeners.size).toBe(0)
    }
  )

  test('lease and subscription cleanup failures remain retryable', async () => {
    const boundary = new InMemoryWebBluetoothTckBoundary()
    const { backend, database, lease, snapshot } = await connectedBoundaryDatabase(boundary)
    const subscription = await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    boundary.failNextNotificationStop(new Error('stop failed'))
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    expect(boundary.resourceSnapshot()).toMatchObject({ notificationStops: 2, notificationListeners: 0 })
    boundary.failNextDisconnect(new Error('disconnect failed'))
    await expect(lease.release()).resolves.toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.disconnectRecord] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )
    expect(lease.connection.state).toBe('disconnected')
    expect(boundary.resourceSnapshot()).toMatchObject({ connected: true, disconnectListeners: 1 })
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    expect(lease.connection.state).toBe('disconnected')
    expect(boundary.resourceSnapshot()).toMatchObject({ connected: false, disconnectCalls: 2, disconnectListeners: 0 })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('ordered owner shares physical notifications and holds the final listener', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const path = snapshot.characteristics[0].path
    const start = jest.spyOn(testFixture.characteristic, 'startNotifications')
    const stop = jest.spyOn(testFixture.characteristic, 'stopNotifications')
    const ordinary = await database.subscribe(path, subscriptionOptions())
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    expect(setup.state).toBe('ready')
    expect(start).toHaveBeenCalledTimes(1)
    await ordinary.remove()
    expect(stop).not.toHaveBeenCalled()
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([9]))
    expect(await orderedRecords(setup.owner, 1)).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([9]) }
    ])
    expect(await setup.owner.close()).toMatchObject([{ memberId: 'a', cleanup: { state: 'released' } }])
    expect(stop).toHaveBeenCalledTimes(1)
    expect(testFixture.notificationListeners.size).toBe(0)
  })

  test('ordered stream awaits real DOM callbacks and close reports cleanup', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const path = snapshot.characteristics[0].path
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    expect(setup.owner.members).toEqual([{ memberId: 'a', path, deliveryMode: 'prefer-notification', effectiveDelivery: 'notification' }])
    const iterator = setup.owner.stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    const value = new Uint8Array([42])
    for (const listener of testFixture.notificationListeners) listener(value)
    value[0] = 99
    expect(await pending).toMatchObject({ done: false, value: {
      kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([42]),
      connectionGeneration: String(database.path.connectionGeneration),
      databaseGeneration: String(database.path.databaseGeneration), delivery: 'notification', provenance: 'read-or-notification'
    } })
    testFixture.setStopFailures(1)
    expect(await setup.owner.stream.close()).toMatchObject({ members: [{ cleanup: { state: 'release-failed' } }] })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(await iterator.next()).toMatchObject({ value: { kind: 'terminal', reason: 'closed' } })
    expect(await setup.owner.stream.close()).toMatchObject({ members: [{ cleanup: { state: 'released' } }] })
  })

  test('ordered overflow automatically cleans up and close retries failed cleanup', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions(null, null, 1))
    testFixture.setStopFailures(1)
    for (const listener of testFixture.notificationListeners) {
      listener(new Uint8Array([1]))
      listener(new Uint8Array([2]))
    }
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(await orderedRecords(setup.owner, 3)).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1]) },
      { kind: 'loss', lostValues: 1, lostBytes: 1 },
      { kind: 'terminal', reason: 'overflow', uncertainTail: true }
    ])
    expect(testFixture.notificationListeners.size).toBe(0)
    expect(await setup.owner.close()).toMatchObject([{ cleanup: { state: 'released' } }])
    expect(backend.resourceCounters().subscriptionConsumers).toBe(0)
  })

  test.each([
    ['owner-released', 'owner-released'],
    ['connection-lost', 'connection-lost'],
    ['adapter-loss', 'source-failed']
  ])('ordered owner reports %s connection end', async (end, expected) => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    await backend.disconnectRecord(lease.record, end)
    expect(await orderedRecords(setup.owner, 1)).toEqual([{ kind: 'terminal', reason: expected, uncertainTail: true }])
  })

  test('ordered explicit close reports closed', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    await setup.owner.close()
    expect(await orderedRecords(setup.owner, 1)).toEqual([{ kind: 'terminal', reason: 'closed', uncertainTail: true }])
  })

  test('ordered acquisition stages alternating DOM callbacks while the second enable is pending', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    const secondBoundary = [...database.characteristicBoundaries.values()][1]
    const secondStart = deferred()
    jest.spyOn(secondBoundary, 'startNotifications').mockImplementationOnce(() => secondStart.promise)
    const opening = backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' },
      { memberId: 'b', path: snapshot.characteristics[1].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions(null, null, 4))
    await flushMicrotasks()
    const [a, b] = [...testFixture.notificationListeners]
    a(new Uint8Array([1]))
    b(new Uint8Array([2]))
    a(new Uint8Array([3]))
    secondStart.resolve()
    const setup = await opening
    expect(setup.state).toBe('ready')
    expect(await orderedRecords(setup.owner, 3)).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1]) },
      { kind: 'value', memberId: 'b', ordinal: '2', value: new Uint8Array([2]) },
      { kind: 'value', memberId: 'a', ordinal: '3', value: new Uint8Array([3]) }
    ])
    await setup.owner.close()
  })

  test('disconnect during ordered setup reports connection loss, not overflow', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const secondBoundary = [...database.characteristicBoundaries.values()][1]
    const secondStart = deferred()
    jest.spyOn(secondBoundary, 'startNotifications').mockImplementationOnce(() => secondStart.promise)
    const opening = backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' },
      { memberId: 'b', path: snapshot.characteristics[1].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    await flushMicrotasks()
    const disconnect = backend.disconnectRecord(lease.record, 'connection-lost')
    const setup = await opening
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'connection.lost' } })
    expect(await orderedRecords(setup.owner, 1)).toEqual([{ kind: 'terminal', reason: 'connection-lost', uncertainTail: true }])
    await disconnect
    secondStart.resolve()
    await flushMicrotasks()
    await setup.owner.close()
  })

  test('rediscovery during ordered setup reports service change, not overflow', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const secondBoundary = [...database.characteristicBoundaries.values()][1]
    const secondStart = deferred()
    jest.spyOn(secondBoundary, 'startNotifications').mockImplementationOnce(() => secondStart.promise)
    const opening = backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' },
      { memberId: 'b', path: snapshot.characteristics[1].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    await flushMicrotasks()
    await expect(backend.gatt.discover(lease.connection, noDeadline())).rejects.toMatchObject({
      normalized: { code: 'gatt.subscribe-failed' }
    })
    secondStart.resolve()
    const setup = await opening
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'gatt.stale-handle' } })
    expect(await orderedRecords(setup.owner, 1)).toEqual([{ kind: 'terminal', reason: 'service-changed', uncertainTail: true }])
    await setup.owner.close()
  })

  test('ordered owner fences its group before a pending final physical stop', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions())
    const capturedListener = [...testFixture.notificationListeners][0]
    const pendingStop = deferred()
    jest.spyOn(testFixture.characteristic, 'stopNotifications').mockImplementationOnce(() => pendingStop.promise)
    const rediscovery = backend.gatt.discover(lease.connection, noDeadline())
    await flushMicrotasks()
    capturedListener(new Uint8Array([8]))
    pendingStop.reject(new Error('stop failed'))
    await expect(rediscovery).rejects.toMatchObject({ normalized: { code: 'gatt.subscribe-failed' } })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(await orderedRecords(setup.owner, 1)).toEqual([{ kind: 'terminal', reason: 'service-changed', uncertainTail: true }])
    expect(await setup.owner.close()).toMatchObject([{ cleanup: { state: 'released' } }])
  })

  test('ordered setup captures callback overflow and returns cleanup owner', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    testFixture.setStartMode('overflow')
    const setup = await backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions(null, null, 1))
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'stream.overflow' }, acquisitions: [{ state: 'acquired' }] })
    expect(await orderedRecords(setup.owner, 3)).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1]) },
      { kind: 'loss', lostValues: 1, lostBytes: 1 },
      { kind: 'terminal', reason: 'overflow', uncertainTail: true }
    ])
    expect(testFixture.notificationListeners.size).toBe(0)
  })

  test('aborted ordered setup retains retryable late-start cleanup', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    testFixture.setStartMode('pending')
    testFixture.setStopFailures(1)
    const controller = new AbortController()
    const opening = backend.gattRuntime.acquireOrdered(database, [
      { memberId: 'a', path: snapshot.characteristics[0].path, deliveryMode: 'prefer-notification' }
    ], subscriptionOptions(controller.signal))
    await flushMicrotasks()
    controller.abort()
    const setup = await opening
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'operation.aborted' }, acquisitions: [{ state: 'indeterminate' }] })
    testFixture.startDeferred.resolve()
    await flushMicrotasks()
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(await setup.owner.close()).toMatchObject([{ cleanup: { state: 'released' } }])
    expect(backend.resourceCounters().subscriptionConsumers).toBe(0)
  })

  test('rediscovery settles old database subscriptions before replacing its generation', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const subscription = await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    const ingress = backend.gattRuntime.notificationIngress(database)
    const group = ingress.register(new Map([[testFixture.characteristic, 'member']]), 4, 16)
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([1]))
    testFixture.setStopFailures(1)

    await expect(backend.gatt.discover(lease.connection, noDeadline())).rejects.toMatchObject({
      normalized: { code: 'gatt.subscribe-failed' }
    })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    await expect(database.snapshot()).resolves.toBeDefined()
    expect(group.drain()).toEqual([
      { kind: 'value', memberId: 'member', ordinal: '1', value: new Uint8Array([1]) }
    ])
    await subscription.remove()
    const resumed = await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([2]))
    expect(group.drain()).toEqual([
      { kind: 'value', memberId: 'member', ordinal: '2', value: new Uint8Array([2]) }
    ])
    await resumed.remove()
    const replacement = await backend.gatt.discover(lease.connection, noDeadline())
    expect(group.drain()).toEqual([{ kind: 'terminal', reason: 'service-changed', uncertainTail: true }])
    expect(replacement.path.databaseGeneration).not.toBe(database.path.databaseGeneration)
  })

  test('keeps admitting remaining member callbacks during failed rediscovery cleanup', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const first = await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    const second = await database.subscribe(snapshot.characteristics[1].path, subscriptionOptions())
    const secondBoundary = [...database.characteristicBoundaries.values()][1]
    const group = backend.gattRuntime.notificationIngress(database).register(new Map([[secondBoundary, 'second']]), 2, 8)
    const pendingStop = deferred()
    jest.spyOn(testFixture.characteristic, 'stopNotifications').mockImplementationOnce(() => pendingStop.promise)
    const discovery = backend.gatt.discover(lease.connection, noDeadline())
    await Promise.resolve()
    await Promise.resolve()
    for (const listener of testFixture.notificationListeners) listener(new Uint8Array([7]))
    pendingStop.reject(new Error('stop failed'))
    await expect(discovery).rejects.toMatchObject({ normalized: { code: 'gatt.subscribe-failed' } })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(group.drain()).toEqual([
      { kind: 'value', memberId: 'second', ordinal: '1', value: new Uint8Array([7]) }
    ])
    await first.remove()
    await second.remove()
  })

  test('destroy quarantines a late chooser completion without retaining a peer, connection, database, or subscription', async () => {
    const boundary = new InMemoryWebBluetoothTckBoundary()
    const backend = await attachedBoundaryBackend(boundary)
    const chooser = selectedPeer(backend)
    await boundary.flush()
    expect(boundary.resourceSnapshot()).toMatchObject({
      chooserRequests: 1,
      pendingChooser: true,
      pageLifecycleListeners: 1
    })

    await expect(backend.destroy()).resolves.toMatchObject({ state: 'release-failed' })
    expect(backend.resourceCounters().chooserSessions).toBe(1)
    expect(boundary.resourceSnapshot()).toMatchObject({
      connected: false,
      disconnectListeners: 0,
      notificationListeners: 0,
      pageLifecycleListeners: 0
    })
    boundary.resolveChooser()
    await expect(chooser).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
    await boundary.flush()
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(backend.resourceCounters()).toMatchObject({
      chooserSessions: 0,
      connectionLeases: 0,
      physicalLinks: 0,
      databaseSnapshots: 0,
      subscriptionConsumers: 0
    })
  })

  test('write copies caller input before awaiting the browser and duplicate paths include descriptors', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, snapshot } = await connectedDatabase(backend)
    expect(snapshot.services).toHaveLength(2)
    expect(snapshot.characteristics).toHaveLength(4)
    expect(snapshot.descriptors).toHaveLength(8)
    const input = new Uint8Array([1, 2])
    const write = database.write(snapshot.characteristics[0].path, input, {
      ...noDeadline(),
      mode: 'with-response'
    })
    input[0] = 9
    await write
    expect(testFixture.written[0]).toEqual([1, 2])
  })

  test('subscription release-failed still calls gatt.disconnect', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    testFixture.setStopFailures(1)
    const result = await lease.release()
    expect(result.state).toBe('release-failed')
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(testFixture.gatt.connected).toBe(false)
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('subscription throw still calls gatt.disconnect', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    testFixture.setStopFailures(1)
    await lease.release()
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expect(testFixture.gatt.connected).toBe(false)
  })

  test('both failures are preserved', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    testFixture.setStopFailures(1)
    testFixture.setDisconnectFailures(1)
    const result = await lease.release()
    expect(result.state).toBe('release-failed')
    const kinds = result.failures.map(failure => failure.resourceKind)
    expect(kinds).toEqual(expect.arrayContaining(['subscription', 'connection']))
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.disconnectRecord] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )
  })

  test('gatt.disconnect failure after successful subscription cleanup is still reported', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    const subscription = await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    await expect(subscription.remove()).resolves.toEqual({ state: 'released', failures: [] })
    testFixture.setDisconnectFailures(1)
    const result = await lease.release()
    expect(result.state).toBe('release-failed')
    expect(result.failures.some(failure => failure.resourceKind === 'connection')).toBe(true)
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.disconnectRecord] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )
  })

  test('every subscription cleanup is attempted before the result is assembled', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    await database.subscribe(snapshot.characteristics[1].path, subscriptionOptions())
    testFixture.setStopFailures(2)
    const result = await lease.release()
    expect(result.state).toBe('release-failed')
    expect(result.failures.filter(failure => failure.resourceKind === 'subscription').length).toBe(2)
    expect(testFixture.gatt.connected).toBe(false)
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
  })

  test('retry repeats only unresolved subscription or physical-disconnect phases', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    testFixture.setStopFailures(1)
    testFixture.setDisconnectFailures(1)
    await expect(lease.release()).resolves.toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.disconnectRecord] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )
    const disconnectsBeforeRetry = testFixture.gatt.connected
    await expect(lease.release()).resolves.toEqual({ state: 'released', failures: [] })
    expect(disconnectsBeforeRetry).toBe(true)
    expect(testFixture.gatt.connected).toBe(false)
  })

  test('local connection and database generations invalidate at terminal disconnect', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease } = await connectedDatabase(backend)
    const generation = lease.connection.connectionGeneration
    await lease.release()
    expect(lease.connection.state).toBe('disconnected')
    expect(lease.connection.connectionGeneration).toBe(generation)
    await expect(database.snapshot()).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
  })

  test('concurrent release and remote disconnect share one terminal outcome', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { lease } = await connectedDatabase(backend)
    const remote = Promise.resolve().then(() => {
      for (const listener of testFixture.disconnectListeners) listener()
    })
    const [releaseResult] = await Promise.all([lease.release(), remote])
    expect(releaseResult.state).toBe('released')
    expect(lease.connection.state === 'disconnected' || lease.connection.state === 'lost').toBe(true)
  })

  test('backend destroy retries unresolved phases and removes listeners and counters', async () => {
    const testFixture = fixture()
    const backend = await backendFixture(testFixture)
    const { database, lease, snapshot } = await connectedDatabase(backend)
    await database.subscribe(snapshot.characteristics[0].path, subscriptionOptions())
    testFixture.setStopFailures(1)
    testFixture.setDisconnectFailures(1)
    await expect(lease.release()).resolves.toMatchObject({ state: 'release-failed' })
    expectConsoleErrorMatching(
      '[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:',
      expect.objectContaining({ message: 'stop failed' })
    )
    expectConsoleErrorMatching(
      '[WebBluetoothBackend.disconnectRecord] Browser disconnect failed:',
      expect.objectContaining({ message: 'disconnect failed' })
    )
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(testFixture.disconnectListeners.size).toBe(0)
    expect(backend.resourceCounters().connectionLeases).toBe(0)
  })
})
