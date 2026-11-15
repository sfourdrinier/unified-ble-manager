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
const { awaitSignal } = require('../helpers/async')

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

async function publicFixture(overrides = {}) {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  if (overrides.writable) {
    boundary.characteristic.properties = { ...boundary.characteristic.properties, write: true }
    delete overrides.writable
  }
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners,
    ...overrides
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
    { filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] },
    { signal: null, deadline: null }
  )
  await boundary.flush()
  boundary.resolveChooser()
  const peer = await choosing
  const connection = await manager.connect(peer.peerId, { signal: null, deadline: null })
  const database = await createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const characteristic = database.characteristic(boundary.serviceUuid, boundary.characteristicUuid)
  let token = 0
  const controller = await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return { boundary, storage, liveOwners, manager, connection, database, characteristic, controller }
}
afterEach(() => jest.restoreAllMocks())

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

async function startRecording(f, id, maxRecords = 20) {
  const start = f.database.reserveOrderedRecordingStart()
  await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
    recording: { id, maxBytes: 1048576, maxRecords }
  })
  return start
}

test.each(['start handle', 'database'])(
  'recording marker %s durably dispatches once and retains authority through a held write',
  async route => {
    const f = await publicFixture({ writable: true })
    let resolveWrite
    const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse').mockImplementation(
      () =>
        new Promise(resolve => {
          resolveWrite = resolve
        })
    )
    const id = route === 'start handle' ? 'live-start' : 'live-database'
    try {
      const start = await startRecording(f, id)
      const marker =
        route === 'start handle' ? start.reserveMarkerWrite() : f.database.reserveOrderedRecordingMarker(id)
      const result = await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
      expect(result).toMatchObject({ state: 'admitted', write: { state: 'started' } })
      expect(f.storage.rows('markerLedger')[0]).toMatchObject({
        request: { bytes: [1] },
        admission: { state: 'admitted' },
        dispatchIntent: true,
        settlement: null
      })
      expect(await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).toMatchObject(result)
      expect(write).toHaveBeenCalledTimes(1)
      await expect(marker.forget()).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
      resolveWrite()
      await awaitSignal(f.controller.stopOrdered(id), 'marker write to settle before recording stop')
      expect(await marker.status()).toMatchObject({ write: { state: 'settled' } })
      const batch = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
      expect(batch.records.map(row => row.record.kind).filter(kind => kind !== 'value')).toEqual(['marker', 'terminal'])
      await marker.forget()
    } finally {
      await f.manager.destroy()
    }
  }
)

test('durable reserve completes before admission or Bluetooth dispatch', async () => {
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'held-reserve')
    const marker = start.reserveMarkerWrite()
    f.storage.pauseNextCompletion()
    const pending = marker.start(f.characteristic, Uint8Array.of(4, 5), 'with-response')
    await f.storage.requestsProcessed()
    expect(f.storage.rows('markerLedger')).toHaveLength(0)
    expect(write).not.toHaveBeenCalled()
    f.storage.complete()
    expect(await pending).toMatchObject({ state: 'admitted' })
    expect(write).toHaveBeenCalledTimes(1)
    await f.controller.stopOrdered('held-reserve')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('lost dispatch-intent response retries the same operation without writing twice', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let lost = true
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      dispatchIntent: async (...params) => {
        const result = await ledger.dispatchIntent(...params)
        if (lost) {
          lost = false
          throw new Error('lost response')
        }
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'lost-intent')
    const marker = start.reserveMarkerWrite()
    await expect(marker.start(f.characteristic, Uint8Array.of(6), 'with-response')).rejects.toThrow('lost response')
    expect(f.storage.rows('markerLedger')[0].dispatchIntent).toBe(true)
    expect(write).not.toHaveBeenCalled()
    expect(await marker.start(f.characteristic, Uint8Array.of(6), 'with-response')).toMatchObject({ state: 'admitted' })
    expect(write).toHaveBeenCalledTimes(1)
    await awaitSignal(f.controller.stopOrdered('lost-intent'), 'lost-intent marker write to settle')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('a held write fences terminal and authority, then settlement wakes ordered draining', async () => {
  const f = await publicFixture({ writable: true })
  let resolveWrite
  const writeDispatched = deferred()
  f.boundary.characteristic.writeValueWithResponse = jest.fn(
    () =>
      new Promise(resolve => {
        resolveWrite = resolve
        writeDispatched.resolve()
      })
  )
  const emit = byte =>
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(byte)
    })
  try {
    const start = await startRecording(f, 'held-order')
    emit(31)
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(9), 'with-response')
    await awaitSignal(writeDispatched.promise, 'held marker write dispatch')
    emit(32)
    expect(await f.controller.stopOrdered('held-order')).toMatchObject({ hostCleanup: { state: 'pending' } })
    expect(f.storage.rows('recordings')[0].terminal).toBe(false)
    expect(f.storage.rows('recordings')[0].markerDebt).toBe(1)
    await expect(marker.start(f.characteristic, Uint8Array.of(10), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invalid-state'
    })
    expect(f.boundary.characteristic.writeValueWithResponse).toHaveBeenCalledTimes(1)
    resolveWrite()
    for (let i = 0; i < 200 && !f.storage.rows('recordings')[0].terminal; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(f.storage.rows('recordings')[0]).toMatchObject({ terminal: true, markerDebt: 0 })
    const batch = await f.controller.prepareOrdered('held-order', { maxItems: 16, maxBytes: 10000 })
    const selected = batch.records.filter(
      row => row.record.kind === 'marker' || (row.record.kind === 'value' && [31, 32].includes(row.record.bytes[0]))
    )
    expect(selected.map(row => (row.record.kind === 'value' ? row.record.bytes[0] : 'marker'))).toEqual([
      31,
      'marker',
      32
    ])
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('a lost settlement response is reconciled without a second Bluetooth write', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let lost = true
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      settle: async (...params) => {
        const result = await ledger.settle(...params)
        if (lost) {
          lost = false
          throw new Error('lost settlement response')
        }
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'lost-settlement')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(11), 'with-response')
    for (let i = 0; i < 200 && lost; i++) await new Promise(resolve => setTimeout(resolve, 0))
    expect(lost).toBe(false)
    await expect(marker.status()).rejects.toThrow('lost settlement response')
    expect(await marker.status()).toMatchObject({ write: { state: 'settled' } })
    expect(write).toHaveBeenCalledTimes(1)
    await f.controller.stopOrdered('lost-settlement')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('source admission and durable dispatch intent each complete before the Bluetooth write', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let releaseAdmission, enteredAdmission, releaseIntent, enteredIntent
  const admissionEntered = new Promise(resolve => {
    enteredAdmission = resolve
  })
  const intentEntered = new Promise(resolve => {
    enteredIntent = resolve
  })
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      admit: async (...params) => {
        const result = await ledger.admit(...params)
        enteredAdmission()
        await new Promise(resolve => {
          releaseAdmission = resolve
        })
        return result
      },
      dispatchIntent: async (...params) => {
        const result = await ledger.dispatchIntent(...params)
        enteredIntent()
        await new Promise(resolve => {
          releaseIntent = resolve
        })
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'barrier-order')
    const marker = start.reserveMarkerWrite()
    const pending = marker.start(f.characteristic, Uint8Array.of(17), 'with-response')
    await admissionEntered
    expect(f.storage.rows('markerLedger')[0]).toMatchObject({ admission: { state: 'admitted' }, dispatchIntent: false })
    expect(write).not.toHaveBeenCalled()
    releaseAdmission()
    await intentEntered
    expect(f.storage.rows('markerLedger')[0]).toMatchObject({ admission: { state: 'admitted' }, dispatchIntent: true })
    expect(write).not.toHaveBeenCalled()
    releaseIntent()
    expect(await pending).toMatchObject({ state: 'admitted' })
    expect(write).toHaveBeenCalledTimes(1)
    await f.controller.stopOrdered('barrier-order')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('cancellation while durable intent is held records not-started and does not write', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let releaseIntent, enteredIntent
  const entered = new Promise(resolve => {
    enteredIntent = resolve
  })
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      dispatchIntent: async (...params) => {
        const result = await ledger.dispatchIntent(...params)
        enteredIntent()
        await new Promise(resolve => {
          releaseIntent = resolve
        })
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'cancel-intent')
    const marker = start.reserveMarkerWrite()
    const cancellation = new AbortController()
    const pending = marker.start(f.characteristic, Uint8Array.of(18), 'with-response', { signal: cancellation.signal })
    await entered
    cancellation.abort()
    releaseIntent()
    expect(await pending).toMatchObject({
      state: 'admitted',
      write: { state: 'not-started', error: { code: 'operation.aborted' } }
    })
    expect(write).not.toHaveBeenCalled()
    expect(f.storage.rows('markerLedger')[0]).toMatchObject({
      dispatchIntent: true,
      settlement: { state: 'not-started' }
    })
    await f.controller.stopOrdered('cancel-intent')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('a lost admission response can resume the exact source acknowledgement and drain', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const originalAck = WebRecordingSourceOwner.prototype.acknowledged
  let enteredAck
  const ackEntered = new Promise(resolve => {
    enteredAck = resolve
  })
  jest.spyOn(WebRecordingSourceOwner.prototype, 'acknowledged').mockImplementation(function (delivery) {
    if (delivery.record.kind === 'marker') enteredAck()
    return originalAck.call(this, delivery)
  })
  let lost = true,
    releaseAdmission,
    enteredAdmission
  const entered = new Promise(resolve => {
    enteredAdmission = resolve
  })
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      admit: async (...params) => {
        const result = await ledger.admit(...params)
        if (lost) {
          enteredAdmission()
          await new Promise(resolve => {
            releaseAdmission = resolve
          })
          lost = false
          throw new Error('lost admission response')
        }
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'lost-admission')
    const marker = start.reserveMarkerWrite()
    const pending = marker.start(f.characteristic, Uint8Array.of(21), 'with-response')
    await entered
    expect(f.storage.rows('records').some(row => row.record.kind === 'marker')).toBe(true)
    await ackEntered
    releaseAdmission()
    await expect(pending).rejects.toThrow('lost admission response')
    expect(await marker.status()).toMatchObject({ state: 'admitted' })
    await awaitSignal(f.controller.stopOrdered('lost-admission'), 'lost admission terminal to commit')
    expect(f.storage.rows('recordings')[0]).toMatchObject({ terminal: true, markerDebt: 0 })
    expect(write.mock.calls.length).toBeLessThanOrEqual(1)
    expect(f.storage.rows('markerLedger')[0]).toMatchObject({
      sourceAck: expect.any(Object),
      settlement: expect.any(Object),
      retired: true
    })
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('a lost source ACK response retries the exact acknowledgement before terminal', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let lost = true
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      acknowledge: async (...params) => {
        const result = await ledger.acknowledge(...params)
        if (lost) {
          lost = false
          throw new Error('lost ack response')
        }
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'lost-ack')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(22), 'with-response')
    for (let i = 0; i < 200 && lost; i++) await new Promise(resolve => setTimeout(resolve, 0))
    expect(lost).toBe(false)
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(f.storage.rows('markerLedger')[0].sourceAck).not.toBeNull()
    await marker.status()
    for (let i = 0; i < 200 && !f.storage.rows('recordings')[0].terminal; i++)
      await new Promise(resolve => setTimeout(resolve, 0))
    expect(f.storage.rows('recordings')[0]).toMatchObject({ terminal: true, markerDebt: 0 })
    expect(f.storage.rows('markerLedger')[0].retired).toBe(true)
    expect(write).toHaveBeenCalledTimes(1)
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test.each(['reserve', 'admit', 'dispatchIntent', 'settle', 'acknowledge', 'retire'])(
  'explicit cancel during held %s preserves the observed device outcome',
  async stage => {
    const ledgerModule = require('../../src/web/web-recording-marker-ledger')
    const original = ledgerModule.createWebRecordingMarkerLedger
    let release, entered
    const held = new Promise(resolve => {
      entered = resolve
    })
    jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
      const ledger = original(...args)
      let once = true
      return {
        ...ledger,
        [stage]: async (...params) => {
          const result = await ledger[stage](...params)
          if (once) {
            once = false
            entered()
            await new Promise(resolve => {
              release = resolve
            })
          }
          return result
        }
      }
    })
    const f = await publicFixture({ writable: true })
    const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse').mockResolvedValue(undefined)
    try {
      const start = await startRecording(f, `explicit-${stage}`)
      const marker = start.reserveMarkerWrite()
      const started = marker.start(f.characteristic, Uint8Array.of(31), 'with-response')
      await held
      const cancelled = marker.cancel()
      release()
      const result = await started
      const cancelResult = await cancelled
      if (['reserve', 'admit', 'dispatchIntent'].includes(stage)) {
        expect(write).not.toHaveBeenCalled()
        if (stage === 'reserve')
          expect(result).toMatchObject({ state: 'not-admitted', error: { code: 'operation.aborted' } })
        else
          expect(result).toMatchObject({
            state: 'admitted',
            write: { state: 'not-started', error: { code: 'operation.aborted' } }
          })
      } else {
        expect(write).toHaveBeenCalledTimes(1)
        expect(cancelResult).toMatchObject({ state: 'admitted', write: { state: 'settled', error: null } })
      }
      await f.controller.stopOrdered(`explicit-${stage}`)
      await marker.forget()
    } finally {
      if (release) release()
      await f.manager.destroy()
    }
  }
)

test('a definitively refused reservation releases local debt and stop publishes terminal', async () => {
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'refused-reservation')
    const marker = start.reserveMarkerWrite()
    await expect(
      marker.start(f.characteristic, new Uint8Array(300000).fill(255), 'with-response')
    ).rejects.toMatchObject({ code: 'stream.quota' })
    expect(write).not.toHaveBeenCalled()
    await marker.forget()
    await expectFinished(f, 'refused-reservation')
  } finally {
    await f.manager.destroy()
  }
})

test('permanently lost intent completion reconciles by operation ID without another write', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let entered, operationId, reconcile
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    reconcile = jest.fn(ledger.reconcile)
    return { ...ledger, reconcile }
  })
  const f = await publicFixture({ writable: true })
  const transaction = f.storage.transaction.bind(f.storage)
  f.storage.transaction = (...args) => {
    const tx = transaction(...args)
    const objectStore = tx.objectStore.bind(tx)
    let dispatch = false,
      complete
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'markerOperations') {
        const add = store.add
        store.add = value => {
          if (JSON.parse(value.signature).kind === 'dispatch') {
            dispatch = true
            operationId = value.operationId
          }
          return add(value)
        }
      }
      return store
    }
    Object.defineProperty(tx, 'oncomplete', {
      set: fn => {
        complete = fn
      },
      get: () => () => {
        if (dispatch) entered()
        else complete?.()
      }
    })
    return tx
  }
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'lost-permanently')
    const marker = start.reserveMarkerWrite()
    const started = marker.start(f.characteristic, Uint8Array.of(32), 'with-response')
    await held
    const result = await within(started, 6500)
    expect(result).toMatchObject({ state: 'admitted' })
    expect(reconcile).toHaveBeenCalledWith(operationId)
    expect(write).toHaveBeenCalledTimes(1)
    expect(await within(marker.status(), 6500)).toMatchObject({ state: 'admitted' })
    await expectFinished(f, 'lost-permanently')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
}, 25000)

test.each([
  [122400, true],
  [122590, false]
])('marker byte boundary %i preserves marker and terminal capacity', async (size, accepted) => {
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'marker-byte-boundary')
    const marker = start.reserveMarkerWrite()
    const outcome = await marker.start(f.characteristic, new Uint8Array(size).fill(255), 'with-response').then(
      value => ({ value }),
      error => ({ error })
    )
    if (outcome.error) {
      expect(accepted).toBe(false)
      expect(outcome.error).toMatchObject({ code: 'stream.quota' })
      await marker.forget()
    } else {
      expect(accepted).toBe(true)
      expect(outcome.value).toMatchObject({ state: 'admitted' })
    }
    await expectFinished(f, 'marker-byte-boundary')
    if (!outcome.error) await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

async function expectFinished(f, id) {
  let stop
  for (let i = 0; i < 200; i++) {
    stop = await f.controller.stopOrdered(id)
    if (stop.recording.state === 'finished' && stop.hostCleanup.state === 'released') return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  expect(stop).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
}

async function within(promise, milliseconds) {
  return awaitSignal(promise, 'bounded marker operation', milliseconds)
}

test('a confirmed reservation transaction abort retires local debt before stop', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let abort = true
  let storage
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((db, ...args) => {
    const ledger = original(db, ...args)
    return {
      ...ledger,
      reserve: (...params) => {
        if (abort) {
          abort = false
          storage.abortNext(new Error('confirmed abort'))
        }
        return ledger.reserve(...params)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  storage = f.storage
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'reservation-abort')
    const marker = start.reserveMarkerWrite()
    await expect(marker.start(f.characteristic, Uint8Array.of(7), 'with-response')).rejects.toThrow('confirmed abort')
    expect(f.storage.rows('markerLedger')).toHaveLength(0)
    expect(write).not.toHaveBeenCalled()
    await marker.forget()
    await expectFinished(f, 'reservation-abort')
  } finally {
    await f.manager.destroy()
  }
})

test('an unknown reservation remains debt during stop until its actual abort is observed', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    let once = true
    return {
      ...ledger,
      reserve: (...params) => {
        if (!once) return ledger.reserve(...params)
        once = false
        return new Promise((resolve, reject) => {
          entered()
          release = () => ledger.reserve(...params).then(resolve, reject)
        })
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'unknown-reservation')
    const marker = start.reserveMarkerWrite()
    const pending = marker.start(f.characteristic, Uint8Array.of(8), 'with-response')
    const rejected = expect(pending).rejects.toThrow('Web recording storage readiness failed')
    await held
    await rejected
    const stop = await f.controller.stopOrdered('unknown-reservation')
    expect(stop.hostCleanup.state).toBe('pending')
    expect(f.storage.rows('recordings')[0].terminal).toBe(false)
    expect(write).not.toHaveBeenCalled()
    release()
    await new Promise(resolve => setTimeout(resolve, 0))
    // The delayed transaction attempted reservation after cleanup began and aborted.
    expect(await marker.status()).toMatchObject({ state: 'not-admitted' })
    await marker.forget()
    await expectFinished(f, 'unknown-reservation')
  } finally {
    await f.manager.destroy()
  }
}, 20000)

test.each(['reserve', 'admit', 'settle', 'acknowledge', 'retire'])(
  'permanently lost %s completion reconciles its exact committed operation',
  async stage => {
    const ledgerModule = require('../../src/web/web-recording-marker-ledger')
    const original = ledgerModule.createWebRecordingMarkerLedger
    let entered, operationId, reconcile
    const held = new Promise(resolve => {
      entered = resolve
    })
    jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
      const ledger = original(...args)
      reconcile = jest.fn(ledger.reconcile)
      let once = true
      return {
        ...ledger,
        reconcile,
        [stage]: async (...params) => {
          const result = await ledger[stage](...params)
          if (once) {
            once = false
            operationId = params[0]
            entered()
            return new Promise(() => {})
          }
          return result
        }
      }
    })
    const f = await publicFixture({ writable: true })
    const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse').mockResolvedValue(undefined)
    try {
      const start = await startRecording(f, `completion-${stage}`)
      const marker = start.reserveMarkerWrite()
      const pending = marker.start(f.characteristic, Uint8Array.of(9), 'with-response')
      await held
      expect(await within(pending, 7000)).toMatchObject({ state: 'admitted' })
      for (let i = 0; i < 700 && !reconcile.mock.calls.some(args => args[0] === operationId); i++)
        await new Promise(resolve => setTimeout(resolve, 10))
      expect(reconcile).toHaveBeenCalledWith(operationId)
      await expectFinished(f, `completion-${stage}`)
      expect(await within(marker.status(), 7000)).toMatchObject({
        state: 'admitted',
        write: { state: 'settled', error: null }
      })
      expect(write).toHaveBeenCalledTimes(1)
      await marker.forget()
    } finally {
      await f.manager.destroy()
    }
  },
  20000
)

test('an abort of a retry cannot erase an earlier unknown reservation commit', async () => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let storage,
    attempt = 0
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: async (...params) => {
        attempt++
        if (attempt === 1) {
          await ledger.reserve(...params)
          throw new Error('reservation reply lost')
        }
        if (attempt === 2) storage.abortNext(new Error('retry transaction aborted'))
        return ledger.reserve(...params)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  storage = f.storage
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'unknown-before-abort')
    const marker = start.reserveMarkerWrite()
    await expect(marker.start(f.characteristic, Uint8Array.of(10), 'with-response')).rejects.toThrow(
      'reservation reply lost'
    )
    await expect(marker.status()).rejects.toThrow('retry transaction aborted')
    expect(f.storage.rows('markerLedger')).toHaveLength(1)
    const stopped = await f.controller.stopOrdered('unknown-before-abort')
    expect(stopped.hostCleanup.state).toBe('pending')
    expect(f.storage.rows('recordings')[0].terminal).toBe(false)
    expect(write).not.toHaveBeenCalled()
    expect(await marker.status()).toMatchObject({ state: 'not-admitted' })
    await marker.forget()
    await expectFinished(f, 'unknown-before-abort')
  } finally {
    await f.manager.destroy()
  }
})

test.each(['mismatched', 'missing request'])('a %s dispatch receipt cannot authorize a write and the exact operation can retry', async corruption => {
  const ledgerModule = require('../../src/web/web-recording-marker-ledger')
  const original = ledgerModule.createWebRecordingMarkerLedger
  let corrupt = true
  jest.spyOn(ledgerModule, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      dispatchIntent: async (...params) => {
        const result = await ledger.dispatchIntent(...params)
        if (corrupt) {
          corrupt = false
          if (corruption === 'missing request') {
            const { request: _request, ...malformed } = result
            return malformed
          }
          return { ...result, request: { ...result.request, bytes: [99] } }
        }
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse').mockResolvedValue(undefined)
  try {
    const start = await startRecording(f, 'bad-intent-receipt')
    const marker = start.reserveMarkerWrite()
    await expect(marker.start(f.characteristic, Uint8Array.of(11), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invariant-violation'
    })
    expect(write).not.toHaveBeenCalled()
    expect(await marker.status()).toMatchObject({ state: 'admitted' })
    expect(write).toHaveBeenCalledTimes(1)
    await expectFinished(f, 'bad-intent-receipt')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test.each(['ready', 'transaction'])('a pre-transaction %s failure releases debt and permits stop', async stage => {
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'pre-open-failure')
    const marker = start.reserveMarkerWrite()
    let fail = true
    if (stage === 'ready')
      f.storage.ready = async () => {
        if (fail) {
          fail = false
          throw new Error('open failed')
        }
      }
    else {
      const transaction = f.storage.transaction.bind(f.storage)
      f.storage.transaction = (...args) => {
        if (fail) {
          fail = false
          throw new Error('open failed')
        }
        return transaction(...args)
      }
    }
    await expect(marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).rejects.toThrow('open failed')
    expect(f.storage.rows('markerLedger')).toHaveLength(0)
    await expectFinished(f, 'pre-open-failure')
    await marker.forget()
  } finally {
    await f.manager.destroy()
  }
})

test('a reservation that completes after reconciliation is unknown wakes a stopped owner', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    let once = true
    return {
      ...ledger,
      reserve: async (...params) => {
        const result = await ledger.reserve(...params)
        if (once) {
          once = false
          entered()
          await new Promise(resolve => {
            release = resolve
          })
        }
        return result
      },
      reconcile: async () => ({ outcome: 'unknown' })
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-unknown')
    const marker = start.reserveMarkerWrite()
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const pending = marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    const result = pending.then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    await jest.advanceTimersByTimeAsync(5001)
    expect(await result).toBe('rejected')
    jest.useRealTimers()
    expect(
      await within(
        marker.cancel().then(
          () => 'resolved',
          () => 'rejected'
        ),
        100
      )
    ).toBe('rejected')
    expect((await f.controller.stopOrdered('late-unknown')).hostCleanup.state).toBe('pending')
    release()
    await expectFinished(f, 'late-unknown')
    expect(write).not.toHaveBeenCalled()
    await marker.forget()
  } finally {
    jest.useRealTimers()
    release?.()
    await f.manager.destroy()
  }
}, 15000)

test('late committed reservation resumes stopped owner without marker calls', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: async (...params) => {
        const result = await ledger.reserve(...params)
        entered()
        await new Promise(resolve => {
          release = resolve
        })
        return result
      },
      reconcile: () => new Promise(() => {})
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-reservation')
    const marker = start.reserveMarkerWrite()
    const pending = marker.start(f.characteristic, Uint8Array.of(1), 'with-response', { timeoutMs: 50 })
    const result = pending.then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    expect(await within(result, 500)).toBe('rejected')
    expect((await f.controller.stopOrdered('late-reservation')).hostCleanup.state).toBe('pending')
    release()
    await expectFinished(f, 'late-reservation')
    expect(write).not.toHaveBeenCalled()
    await marker.forget()
  } finally {
    release?.()
    await f.manager.destroy()
  }
}, 15000)

test('a reservation completing as reconciliation reports unknown still wakes the stopped owner', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    let once = true
    return {
      ...ledger,
      reserve: async (...params) => {
        const result = await ledger.reserve(...params)
        if (once) {
          once = false
          entered()
          await new Promise(resolve => {
            release = resolve
          })
        }
        return result
      },
      reconcile: async () => {
        queueMicrotask(() => release())
        return { outcome: 'unknown' }
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-unknown-race')
    const marker = start.reserveMarkerWrite()
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const pending = marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    const result = pending.then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    expect((await f.controller.stopOrdered('late-unknown-race')).hostCleanup.state).toBe('pending')
    await jest.advanceTimersByTimeAsync(5001)
    await result
    jest.useRealTimers()
    await expectFinished(f, 'late-unknown-race')
    expect(write).not.toHaveBeenCalled()
    await marker.forget()
  } finally {
    jest.useRealTimers()
    release?.()
    await f.manager.destroy()
  }
}, 15000)

test('deadline and cancellation promptly bound waiters while reservation outcome is unknown', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: (...params) =>
        new Promise((resolve, reject) => {
          entered()
          release = () => ledger.reserve(...params).then(resolve, reject)
        }),
      reconcile: () => new Promise(() => {})
    }
  })
  const f = await publicFixture({ writable: true })
  try {
    const start = await startRecording(f, 'bounded-marker')
    const marker = start.reserveMarkerWrite()
    const pending = marker.start(f.characteristic, Uint8Array.of(1), 'with-response', { timeoutMs: 50 })
    const result = pending.then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    expect(await within(result, 500)).toBe('rejected')
    expect(
      await within(
        marker.cancel().then(
          () => 'resolved',
          () => 'rejected'
        ),
        500
      )
    ).toBe('rejected')
    expect((await f.controller.stopOrdered('bounded-marker')).hostCleanup.state).toBe('pending')
    release()
    await expectFinished(f, 'bounded-marker')
  } finally {
    release?.()
    await f.manager.destroy()
  }
}, 15000)

test.each(['version', 'recordingId', 'producerIncarnation'])(
  'wrong %s in dispatch receipt never authorizes Bluetooth',
  async field => {
    const module = require('../../src/web/web-recording-marker-ledger')
    const original = module.createWebRecordingMarkerLedger
    jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
      const ledger = original(...args)
      return {
        ...ledger,
        dispatchIntent: async (...params) => ({
          ...(await ledger.dispatchIntent(...params)),
          [field]: field === 'version' ? 2 : 'wrong'
        })
      }
    })
    const f = await publicFixture({ writable: true })
    const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
    try {
      const start = await startRecording(f, `bad-${field}`)
      const marker = start.reserveMarkerWrite()
      await expect(marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).rejects.toMatchObject({
        code: 'lifecycle.invariant-violation'
      })
      expect(write).not.toHaveBeenCalled()
    } finally {
      await f.manager.destroy()
    }
  }
)

test('a reconciled dispatch with a mismatched operation signature cannot authorize Bluetooth', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let storage
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      dispatchIntent: async (...params) => {
        await ledger.dispatchIntent(...params)
        storage.mutate('markerOperations', row => {
          if (row.operationId === params[0]) row.signature = 'wrong'
        })
        return new Promise(() => {})
      }
    }
  })
  const f = await publicFixture({ writable: true })
  storage = f.storage
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'bad-signature')
    const marker = start.reserveMarkerWrite()
    await expect(marker.start(f.characteristic, Uint8Array.of(1), 'with-response')).rejects.toMatchObject({
      code: 'lifecycle.invariant-violation'
    })
    expect(write).not.toHaveBeenCalled()
  } finally {
    await f.manager.destroy()
  }
}, 15000)

test.each(['漢'.repeat(1000), '\u0001'.repeat(1000), '😀'.repeat(1000)])(
  'write failure evidence fits serialized byte bound %#',
  async message => {
    const f = await publicFixture({ writable: true })
    const failure = new Error(message)
    jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse').mockRejectedValue(failure)
    try {
      const start = await startRecording(f, 'unicode-evidence')
      const marker = start.reserveMarkerWrite()
      await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
      await expectFinished(f, 'unicode-evidence')
      const row = f.storage.rows('markerLedger')[0]
      expect(row.settlement).toMatchObject({ state: 'settled', outcome: 'failure' })
      expect(Buffer.byteLength(JSON.stringify(row.settlement.evidence))).toBeLessThanOrEqual(2048)
      expect((await marker.status()).write.error).not.toBeNull()
      await marker.forget()
    } finally {
      await f.manager.destroy()
    }
  }
)

test('a rejected reconciliation followed by the original late success releases a stopped owner', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let release, entered
  const held = new Promise(resolve => { entered = resolve })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: async (...params) => {
        const result = await ledger.reserve(...params)
        entered()
        await new Promise(resolve => { release = resolve })
        return result
      },
      reconcile: async () => { throw new Error('reconciliation storage failure') }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-original-success')
    const marker = start.reserveMarkerWrite()
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const outcome = marker.start(f.characteristic, Uint8Array.of(3), 'with-response').then(
      () => 'resolved', error => error
    )
    await held
    expect((await f.controller.stopOrdered('late-original-success')).hostCleanup.state).toBe('pending')
    await jest.advanceTimersByTimeAsync(5001)
    expect(await outcome).toMatchObject({ message: expect.stringContaining('reconciliation storage failure') })
    release()
    jest.useRealTimers()
    // Observe durable state only: no second stop, status, or marker call may
    // drive the owner after the original completion arrives.
    for (let i = 0; i < 200; i++) {
      const row = f.storage.rows('recordings')[0]
      const authority = f.storage.rows('authorityReservations')[0]
      if (row.terminal && row.markerDebt === 0 && authority.phase === 'released') break
      await new Promise(resolve => setTimeout(resolve, 0))
    }
    expect(write).not.toHaveBeenCalled()
    expect(f.storage.rows('recordings')[0]).toMatchObject({ terminal: true, markerDebt: 0 })
    expect(f.storage.rows('authorityReservations')[0]).toMatchObject({ phase: 'released' })
  } finally {
    jest.useRealTimers()
    release?.()
    await f.manager.destroy()
  }
}, 15000)

test('late committed reconciliation resumes a stopped owner without further marker calls', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let releaseReconcile, entered
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: async (...params) => {
        await ledger.reserve(...params)
        entered()
        return new Promise(() => {})
      },
      reconcile: async (...params) => {
        const result = await ledger.reconcile(...params)
        await new Promise(resolve => {
          releaseReconcile = resolve
        })
        return result
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-reconcile')
    const marker = start.reserveMarkerWrite()
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const pending = marker.start(f.characteristic, Uint8Array.of(3), 'with-response')
    const outcome = pending.then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    expect((await f.controller.stopOrdered('late-reconcile')).hostCleanup.state).toBe('pending')
    await jest.advanceTimersByTimeAsync(10001)
    expect(await outcome).toBe('rejected')
    releaseReconcile()
    jest.useRealTimers()
    await expectFinished(f, 'late-reconcile')
    expect(write).not.toHaveBeenCalled()
  } finally {
    jest.useRealTimers()
    releaseReconcile?.()
    await f.manager.destroy()
  }
}, 15000)

test('a late uncertain reservation failure resumes a stopped owner without marker calls', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let rejectReservation,
    entered,
    first = true
  const held = new Promise(resolve => {
    entered = resolve
  })
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      reserve: async (...params) => {
        const result = await ledger.reserve(...params)
        if (!first) return result
        first = false
        entered()
        return new Promise((_, reject) => {
          rejectReservation = reject
        })
      },
      reconcile: async () => ({ outcome: 'unknown' })
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'late-reservation-error')
    const marker = start.reserveMarkerWrite()
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const outcome = marker.start(f.characteristic, Uint8Array.of(3), 'with-response').then(
      () => 'resolved',
      () => 'rejected'
    )
    await held
    expect((await f.controller.stopOrdered('late-reservation-error')).hostCleanup.state).toBe('pending')
    await jest.advanceTimersByTimeAsync(5001)
    expect(await outcome).toBe('rejected')
    rejectReservation(new Error('reply lost'))
    jest.useRealTimers()
    await expectFinished(f, 'late-reservation-error')
    expect(write).not.toHaveBeenCalled()
  } finally {
    jest.useRealTimers()
    rejectReservation?.(new Error('test cleanup'))
    await f.manager.destroy()
  }
}, 15000)

test('cancellation bounds a fresh settlement retry after its failure was observed', async () => {
  const module = require('../../src/web/web-recording-marker-ledger')
  const original = module.createWebRecordingMarkerLedger
  let calls = 0,
    release
  const firstSettlement = deferred()
  const retrySettlement = deferred()
  jest.spyOn(module, 'createWebRecordingMarkerLedger').mockImplementation((...args) => {
    const ledger = original(...args)
    return {
      ...ledger,
      settle: async (...params) => {
        calls++
        if (calls === 1) {
          firstSettlement.resolve()
          throw new Error('settlement failed')
        }
        retrySettlement.resolve()
        await new Promise(resolve => {
          release = resolve
        })
        return ledger.settle(...params)
      }
    }
  })
  const f = await publicFixture({ writable: true })
  const write = jest.spyOn(f.boundary.characteristic, 'writeValueWithResponse')
  try {
    const start = await startRecording(f, 'cancel-retry')
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(4), 'with-response')
    await awaitSignal(firstSettlement.promise, 'first marker settlement failure')
    await expect(marker.status()).rejects.toThrow('settlement failed')
    const cancelled = marker.cancel().then(
      () => 'resolved',
      () => 'rejected'
    )
    await awaitSignal(retrySettlement.promise, 'marker settlement retry')
    expect(calls).toBe(2)
    expect(await within(cancelled, 100)).toBe('rejected')
    expect(write).toHaveBeenCalledTimes(1)
    release?.()
    await expectFinished(f, 'cancel-retry')
  } finally {
    release?.()
    await f.manager.destroy()
  }
}, 15000)
