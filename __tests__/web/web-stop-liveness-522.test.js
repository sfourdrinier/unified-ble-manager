const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createPublicGattDatabase } = require('../../src/public/gatt')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { webcrypto } = require('crypto')

function locks() {
  const held = new Set()
  return {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
}

async function publicFixture() {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const install = liveOwners.install.bind(liveOwners)
  let owner
  liveOwners.install = (identity, value) => {
    owner = value
    install(identity, value)
  }
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  const attachedBackend = await attachBleBackend(backend, provider.descriptor.compatibility)
  const manager = await createBleManager(
    {
      attachedBackend,
      clientId: opaqueId('public-recording-client', 'client', 'web'),
      managerId: opaqueId('public-recording-manager', 'manager', 'web'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attachedBackend),
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => boundary.now() }
  )
  const choosing = backend.choose(
    {
      filters: [],
      acceptAllDevices: true,
      optionalServices: [boundary.serviceUuid, boundary.instances[1].serviceUuid]
    },
    { signal: null, deadline: null }
  )
  await boundary.flush()
  boundary.resolveChooser()
  const peer = await choosing
  const connection = await manager.connect(peer.peerId, { signal: null, deadline: null })
  const coreDatabase = await connection.discover({ signal: null, deadline: null })
  const database = await createPublicGattDatabase(coreDatabase)
  const characteristic = database.characteristic(boundary.serviceUuid, boundary.characteristicUuid)
  let token = 0
  const controller = await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return {
    boundary,
    storage,
    liveOwners,
    manager,
    connection,
    database,
    coreDatabase,
    characteristic,
    controller,
    get owner() {
      return owner
    }
  }
}

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

async function until(predicate, label) {
  for (let i = 0; i < 200; i++) {
    if (predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error(`condition did not settle: ${label}`)
}

function emit(f, size, instance = f.boundary.instances[0]) {
  f.boundary.emitNotification({
    serviceUuid: instance.serviceUuid,
    serviceOccurrence: instance.serviceOccurrence,
    characteristicUuid: instance.characteristicUuid,
    characteristicOccurrence: instance.characteristicOccurrence,
    value: new Uint8Array(size)
  })
}

const limits = { maxItems: 20, maxBytes: 20000 }

test('public ordered stream translates synchronous marker reservation failures', async () => {
  const f = await publicFixture()
  try {
    const { contractError } = require('../../src/backend-contract/errors')
    const { BleError } = require('../../src/public/errors')
    const source = Object.create(f.coreDatabase)
    source.subscribeOrdered = async () => ({
      state: 'ready',
      members: [],
      stream: {
        limits: {}, close: async () => {}, discard: async () => {},
        reserveMarkerWrite() { throw contractError('lifecycle.invalid-state', 'gatt', 'test.marker') },
        [Symbol.asyncIterator]() { return { next: async () => ({ done: true }) } }
      }
    })
    const database = await createPublicGattDatabase(source)
    const characteristic = database.characteristic(f.boundary.serviceUuid, f.boundary.characteristicUuid)
    const setup = await database.subscribeOrdered([{ memberId: 'heart', characteristic }])
    let failure
    try { setup.stream.reserveMarkerWrite() } catch (error) { failure = error }
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.code).toBe('lifecycle.invalid-state')
  } finally { await f.manager.destroy() }
})

test.each(['abort', 'deadline'])('public subscription does not start after %s before physical dispatch', async event => {
  const f = await publicFixture()
  const abort = new AbortController()
  const add = f.boundary.characteristic.addNotificationListener
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => event === 'abort' ? abort.abort() : f.boundary.advanceTime(10))
  }
  try {
    await expect(f.characteristic.subscribe({ signal: abort.signal, timeoutMs: 10 }))
      .rejects.toMatchObject({ code: event === 'abort' ? 'operation.aborted' : 'operation.timed-out' })
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    expect(f.boundary.notificationStarts).toBe(0)
  } finally {
    await f.manager.destroy()
  }
})

test('public cancelled ordered setup retains stream and rejected close for retry', async () => {
  const f = await publicFixture()
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const { contractError } = require('../../src/backend-contract/errors')
  const { BleError } = require('../../src/public/errors')
  const abort = new AbortController()
  const fenceFailure = contractError('adapter.resetting', 'stream', 'test.fence')
  let fences = 0
  const stream = new CoreOrderedNotificationStream(
    { itemCapacity: 4, byteCapacity: 1024, reservedControlCapacity: 1 },
    () => { if (++fences <= 3) throw fenceFailure },
    async () => ({ group: { state: 'released', failures: [] }, members: [] })
  )
  stream.emit({ kind: 'value', collectionId: 'c', connectionGeneration: 'g', databaseGeneration: 'd',
    memberId: 'heart', ordinal: '1', value: Uint8Array.of(7), delivery: 'unknown', provenance: 'unknown' })
  f.coreDatabase.database.backendDatabase.subscribeOrdered = async members => {
    abort.abort()
    return { state: 'ready', members, stream }
  }
  try {
    const setup = await f.database.subscribeOrdered([{ memberId: 'heart', characteristic: f.characteristic }], { signal: abort.signal })
    expect(setup).toMatchObject({ state: 'failed', error: { code: 'operation.aborted' },
      cleanup: { group: { state: 'release-failed', failures: [{ error: { code: 'adapter.resetting', operation: 'test.fence' } }] } } })
    const iterator = setup.stream[Symbol.asyncIterator]()
    expect(await iterator.next()).toMatchObject({ value: { kind: 'value', ordinal: '1', value: Uint8Array.of(7) } })
    expect(await iterator.next()).toMatchObject({ value: { kind: 'terminal', fenceError: { code: 'adapter.resetting' } } })
    expect(() => setup.stream[Symbol.asyncIterator]()).toThrow(BleError)
    for (const close of [() => setup.close(), () => setup.stream.close()]) {
      const error = await close().catch(value => value)
      expect(error).toBeInstanceOf(BleError)
      expect(error).toMatchObject({ code: 'adapter.resetting', operation: 'test.fence', recovery: expect.anything() })
    }
    expect(await setup.close()).toMatchObject({ group: { state: 'released' } })
    expect(fences).toBe(4)
  } finally { await f.manager.destroy() }
})

test.each(['discard', 'return'])('public ordered iterator and %s translate internal failures', async action => {
  const f = await publicFixture()
  const { CoreOrderedNotificationStream } = require('../../src/core/ordered-notification-stream')
  const { contractError } = require('../../src/backend-contract/errors')
  const { BleError } = require('../../src/public/errors')
  let fences = 0
  const stream = new CoreOrderedNotificationStream(
    { itemCapacity: 4, byteCapacity: 1024, reservedControlCapacity: 1 },
    () => { if (++fences === 1) throw contractError('adapter.resetting', 'stream', 'test.discard') },
    async () => ({ group: { state: 'released', failures: [] }, members: [] })
  )
  f.coreDatabase.database.backendDatabase.subscribeOrdered = async members => ({ state: 'ready', members, stream })
  try {
    const setup = await f.database.subscribeOrdered([{ memberId: 'heart', characteristic: f.characteristic }])
    const iterator = setup.stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    const concurrent = await iterator.next().catch(value => value)
    expect(concurrent).toBeInstanceOf(BleError)
    expect(concurrent).toMatchObject({ code: 'lifecycle.invariant-violation', operation: 'ordered-notification-stream.concurrent-next' })
    const error = await (action === 'discard' ? setup.stream.discard() : iterator.return()).catch(value => value)
    expect(error).toBeInstanceOf(BleError)
    expect(error).toMatchObject({ code: 'adapter.resetting', operation: 'test.discard' })
    await pending
    expect(await setup.stream.discard()).toMatchObject({ group: { state: 'released' } })
  } finally { await f.manager.destroy() }
})

test.each(['abort', 'deadline'])('public ordered subscription does not start after %s before dispatch', async event => {
  const f = await publicFixture()
  const abort = new AbortController()
  const add = f.boundary.characteristic.addNotificationListener
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => event === 'abort' ? abort.abort() : f.boundary.advanceTime(10))
  }
  try {
    const setup = await f.database.subscribeOrdered(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { signal: abort.signal, timeoutMs: 10 }
    )
    expect(setup).toMatchObject({ state: 'failed', error: { code: event === 'abort' ? 'operation.aborted' : 'operation.timed-out' } })
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    expect(f.boundary.notificationStarts).toBe(0)
    await setup.stream.close()
  } finally {
    await f.manager.destroy()
  }
})

test.each(['abort', 'deadline'])('a live public ordered consumer can authorize a shared start after another reaches its %s', async event => {
  const f = await publicFixture()
  const abort = new AbortController()
  const add = f.boundary.characteristic.addNotificationListener
  let second
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => {
      second = f.database.subscribeOrdered([{ memberId: 'live', characteristic: f.characteristic }])
      if (event === 'abort') abort.abort()
      else f.boundary.advanceTime(10)
    })
  }
  try {
    const first = await f.characteristic.subscribe({ signal: abort.signal, timeoutMs: 10 }).catch(error => error)
    const live = await second
    expect(first).toMatchObject({ code: event === 'abort' ? 'operation.aborted' : 'operation.timed-out' })
    expect(live.state).toBe('ready')
    expect(f.boundary.notificationStarts).toBe(1)
    await live.stream.close()
  } finally {
    await f.manager.destroy()
  }
})

test('a deadline passing after dispatch does not replace the physical start result', async () => {
  const f = await publicFixture()
  const held = deferred()
  const start = f.boundary.characteristic.startNotifications
  let entered = false
  f.boundary.characteristic.startNotifications = async () => {
    entered = true
    await held.promise
    return start()
  }
  try {
    const starting = f.characteristic.subscribe({ timeoutMs: 10 })
    await until(() => entered, 'physical start')
    f.boundary.advanceTime(10)
    held.resolve()
    const subscription = await starting
    expect(f.boundary.notificationStarts).toBe(1)
    await subscription.remove()
  } finally {
    held.resolve()
    await f.manager.destroy()
  }
})

test('a later shared consumer cannot borrow an earlier live consumer after abort', async () => {
  const f = await publicFixture()
  const abort = new AbortController()
  const add = f.boundary.characteristic.addNotificationListener
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => abort.abort())
  }
  try {
    const livePromise = f.characteristic.subscribe()
    const cancelledPromise = f.characteristic.subscribe({ signal: abort.signal, timeoutMs: 10 }).catch(error => error)
    const live = await livePromise
    const cancelled = await cancelledPromise
    expect(cancelled).toMatchObject({ code: 'operation.aborted' })
    expect(f.boundary.notificationStarts).toBe(1)
    await live.remove()
  } finally {
    await f.manager.destroy()
  }
})

test('a fenced public recording and aborted shared consumer do not start notifications', async () => {
  const f = await publicFixture()
  const abort = new AbortController()
  const add = f.boundary.characteristic.addNotificationListener
  let other
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => {
      other = f.database.subscribeOrdered([{ memberId: 'cancelled', characteristic: f.characteristic }], { signal: abort.signal })
      abort.abort()
      // Place the source fence in the dispatch gap; the API below observes cleanup.
      f.owner.supervisor.stop()
    })
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'recorded', characteristic: f.characteristic }],
      { recording: { id: 'fenced-shared-start', maxBytes: 1048576, maxRecords: 20 } }
    )
    const cancelled = await other
    expect(cancelled).toMatchObject({ state: 'failed', error: { code: 'operation.aborted' } })
    expect(f.boundary.notificationStarts).toBe(0)
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    expectConsoleErrorMatching('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    await cancelled.stream.close()
    await f.controller.stopOrdered('fenced-shared-start')
  } finally {
    await f.manager.destroy()
  }
})

test('a fenced recording does not acquire a shared start authorized by a live consumer', async () => {
  const f = await publicFixture()
  const add = f.boundary.characteristic.addNotificationListener
  let other
  f.boundary.characteristic.addNotificationListener = listener => {
    add(listener)
    queueMicrotask(() => {
      other = f.database.subscribeOrdered([{ memberId: 'live', characteristic: f.characteristic }])
      f.owner.supervisor.stop()
    })
  }
  try {
    const result = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'recorded', characteristic: f.characteristic }],
      { recording: { id: 'fenced-live', maxBytes: 1048576, maxRecords: 20 } }
    )
    const live = await other
    expect(live.state).toBe('ready')
    expect(f.boundary.notificationStarts).toBe(1)
    expect(result).toMatchObject({ kind: 'started', receipt: {
      state: 'setup-failed', setupFailure: { code: 'lifecycle.invalid-state' },
      members: [{ acquisition: 'indeterminate' }]
    } })
    expect(f.owner.supervisor.status().members[0]).toMatchObject({
      acquisition: 'failed', acquisitionFailure: { normalized: { code: 'lifecycle.invalid-state' } }
    })
    await live.stream.close()
    await f.controller.stopOrdered('fenced-live')
  } finally {
    await f.manager.destroy()
  }
})

test('public acknowledgement frees a full journal and drains late observations while physical stop is held', async () => {
  const f = await publicFixture()
  const held = deferred()
  let stops = 0
  const original = f.boundary.characteristic.stopNotifications
  f.boundary.characteristic.stopNotifications = async () => {
    stops++
    await held.promise
    return original()
  }
  let stopping
  try {
    await f.database.reserveOrderedRecordingStart().start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: { id: 'held-capacity', maxBytes: 1048576, maxRecords: 1 }
    })
    await until(() => f.storage.rows('records').length === 1)
    stopping = f.controller.stopOrdered('held-capacity')
    await until(() => stops === 1)
    emit(f, 3)
    emit(f, 4)
    await until(() => f.owner.supervisor.status().state === 'waiting-capacity')
    const first = await f.controller.prepareOrdered('held-capacity', limits)
    let acknowledged = false
    const ack = f.controller.acknowledge('held-capacity', first.token).then(result => {
      acknowledged = true
      return result
    })
    await until(() => acknowledged, 'acknowledgement')
    await ack
    await until(() => f.storage.rows('records').some(row => row.record.kind === 'failure-observation'))
    expect(f.storage.rows('records').map(row => row.record.kind)).toEqual(['failure-observation'])
    expect(f.storage.rows('records')[0].record.sourceRejection.bytes).toBe('3')
    expect(stops).toBe(1)
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false })
    const second = await f.controller.prepareOrdered('held-capacity', limits)
    await f.controller.acknowledge('held-capacity', second.token)
    await until(() => f.storage.rows('records').some(row => row.record.sourceRejection?.bytes === '4'))
    const third = await f.controller.prepareOrdered('held-capacity', limits)
    await f.controller.acknowledge('held-capacity', third.token)
    held.resolve()
    await stopping
    await until(() => f.storage.rows('records').some(row => row.record.kind === 'terminal'), 'terminal')
    const terminal = await f.controller.prepareOrdered('held-capacity', limits)
    expect(terminal.records.map(row => row.record.kind)).toEqual(['terminal'])
    const ids = [...first.records, ...second.records, ...third.records, ...terminal.records].map(row =>
      Number(row.metadata.deliveryId)
    )
    expect(ids).toEqual([1, 2, 3, 4])
    await f.controller.acknowledge('held-capacity', terminal.token)
  } finally {
    held.resolve()
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})

test('public stop dispatches independent releases, retains late callbacks, and retries only the failed lease', async () => {
  const f = await publicFixture()
  const firstHeld = deferred()
  const secondHeld = deferred()
  const secondInstance = f.boundary.instances[1]
  const second = f.database.characteristic(secondInstance.serviceUuid, secondInstance.characteristicUuid, {
    serviceOccurrence: 0,
    characteristicOccurrence: 0
  })
  const firstOriginal = f.boundary.characteristic.stopNotifications
  const secondOriginal = secondInstance.characteristic.stopNotifications
  let firstCalls = 0,
    secondCalls = 0
  f.boundary.characteristic.stopNotifications = async () => {
    firstCalls++
    await firstHeld.promise
    return firstOriginal()
  }
  secondInstance.characteristic.stopNotifications = async () => {
    secondCalls++
    if (secondCalls === 1) await secondHeld.promise
    return secondOriginal()
  }
  let stopping
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [
        { memberId: 'heart', characteristic: f.characteristic },
        { memberId: 'battery', characteristic: second }
      ],
      { recording: { id: 'independent-stops', maxBytes: 1048576, maxRecords: 20 } }
    )
    stopping = f.controller.stopOrdered('independent-stops')
    await until(() => firstCalls === 1)
    await until(() => secondCalls === 1, 'second release')
    emit(f, 3, secondInstance)
    await until(() => f.storage.rows('records').some(row => row.record.sourceRejection?.bytes === '3'))
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false })
    const firstBatch = await f.controller.prepareOrdered('independent-stops', { ...limits, maxItems: 1 })
    await f.controller.acknowledge('independent-stops', firstBatch.token)
    expect(firstCalls).toBe(1)
    expect(secondCalls).toBe(1)
    const failure = new Error('second stop failed')
    secondHeld.reject(failure)
    await until(() => f.owner.supervisor.status().cleanupFailure !== null)
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    expect(firstCalls).toBe(1)
    expect(secondCalls).toBe(1)
    emit(f, 4, secondInstance)
    const prefix = await f.controller.prepareOrdered('independent-stops', { ...limits, maxItems: 1 })
    await f.controller.acknowledge('independent-stops', prefix.token)
    await until(() => secondCalls === 2, 'failed release retry')
    await until(() => f.storage.rows('records').some(row => row.record.sourceRejection?.bytes === '4'), 'late callback')
    expect(firstCalls).toBe(1)
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false })
    firstHeld.resolve()
    expect(await stopping).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    expect(await f.controller.stopOrdered('independent-stops')).toMatchObject({
      recording: { state: 'finished' },
      hostCleanup: { state: 'released' }
    })
    expect(firstCalls).toBe(1)
    expect(secondCalls).toBe(2)
    const batch = await f.controller.prepareOrdered('independent-stops', limits)
    expect(batch.records.map(row => row.record.kind)).toEqual(['failure-observation', 'terminal'])
    expect(prefix.records[0].record.sourceRejection.bytes).toBe('3')
    expect(batch.records[0].record.sourceRejection.bytes).toBe('4')
    expect(
      [...firstBatch.records, ...prefix.records, ...batch.records].map(row => Number(row.metadata.deliveryId))
    ).toEqual([1, 2, 3, 4])
    await f.controller.acknowledge('independent-stops', batch.token)
  } finally {
    firstHeld.resolve()
    secondHeld.resolve()
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})

test('cancelled start waiter and late enable do not lose independent stop cleanup', async () => {
  const f = await publicFixture()
  const enableHeld = deferred()
  const stopHeld = deferred()
  const secondInstance = f.boundary.instances[1]
  const second = f.database.characteristic(secondInstance.serviceUuid, secondInstance.characteristicUuid, {
    serviceOccurrence: 0,
    characteristicOccurrence: 0
  })
  const firstStop = f.boundary.characteristic.stopNotifications
  const secondEnable = secondInstance.characteristic.startNotifications
  const secondStop = secondInstance.characteristic.stopNotifications
  let enableCalls = 0
  let firstStops = 0
  let secondStops = 0
  secondInstance.characteristic.startNotifications = async () => {
    enableCalls++
    await enableHeld.promise
    return secondEnable()
  }
  f.boundary.characteristic.stopNotifications = async () => {
    firstStops++
    await stopHeld.promise
    return firstStop()
  }
  secondInstance.characteristic.stopNotifications = async () => {
    secondStops++
    return secondStop()
  }
  let stopping
  try {
    const abort = new AbortController()
    const handle = f.database.reserveOrderedRecordingStart()
    const starting = handle.start(
      [
        { memberId: 'heart', characteristic: f.characteristic },
        { memberId: 'battery', characteristic: second }
      ],
      { recording: { id: 'cancelled-waiter', maxBytes: 1048576, maxRecords: 20 }, signal: abort.signal }
    )
    await until(() => enableCalls === 1, 'second enable')
    abort.abort()
    await expect(starting).rejects.toMatchObject({ code: 'operation.aborted' })
    stopping = f.controller.stopOrdered('cancelled-waiter')
    await until(() => firstStops === 1, 'first release')
    emit(f, 5)
    expect(secondStops).toBe(0)
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false })
    enableHeld.resolve()
    await until(() => secondStops === 1, 'late enable cleanup')
    await until(() => f.storage.rows('records').some(row => row.record.sourceRejection?.bytes === '5'), 'late callback')
    expect(firstStops).toBe(1)
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false })
    stopHeld.resolve()
    await stopping
    await until(() => f.owner.supervisor.status().authorityReleased, 'authority release')
    const batch = await f.controller.prepareOrdered('cancelled-waiter', limits)
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'failure-observation', 'terminal'])
    expect(batch.records[1].record.sourceRejection.bytes).toBe('5')
    expect(firstStops).toBe(1)
    expect(secondStops).toBe(1)
    await f.controller.acknowledge('cancelled-waiter', batch.token)
  } finally {
    enableHeld.resolve()
    stopHeld.resolve()
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})

test('public recording stopped during another subscription stop never enables notifications', async () => {
  const f = await publicFixture()
  const held = deferred()
  const originalStop = f.boundary.characteristic.stopNotifications
  let stopEntered = false
  let stopping, removing, starting
  try {
    const ordinary = await f.characteristic.subscribe()
    f.boundary.characteristic.stopNotifications = async () => {
      stopEntered = true
      await held.promise
      return originalStop()
    }
    removing = ordinary.remove()
    await until(() => stopEntered, 'ordinary physical stop')
    starting = f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'stop-before-enable', maxBytes: 1048576, maxRecords: 20 } }
    ).catch(error => error)
    await until(() => f.owner?.supervisor.status().members.some(member => member.acquisition === 'enabling'), 'recording waiting')
    stopping = f.controller.stopOrdered('stop-before-enable')
    await until(() => f.owner.supervisor.status().terminalReason !== null, 'recording fenced')
    held.resolve()
    await removing
    await starting
    await stopping
    expect(f.boundary.notificationStarts).toBe(1)
    await f.controller.stopOrdered('stop-before-enable')
    await until(() => f.owner.supervisor.status().authorityReleased, 'recording cleanup')
  } finally {
    held.resolve()
    await removing?.catch(() => {})
    await starting?.catch(() => {})
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})
