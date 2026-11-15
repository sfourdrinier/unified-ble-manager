// __tests__/ipc/ordered-recording-startup.test.js
'use strict'

const { IpcGattDatabase } = require('../../src/ipc/manager')

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

async function fixture(route) {
  const manager = {
    bootstrap: {
      attachment: {
        attachmentId: 'attachment',
        backendInstanceId: 'backend',
        backendGeneration: '1',
        adapter: { adapterId: 'adapter', adapterGeneration: '1' }
      },
      rendererLease: { leaseId: 'lease' },
      versions: { ipcProtocol: { selected: { value: 9 } } }
    },
    nextOrderedHandle: 0,
    route
  }
  const connection = {
    handle: 'connection',
    peerId: 'peer',
    connectionId: 'connection',
    ownerLeaseId: 'owner',
    connectionGeneration: '1',
    registerDatabase() {},
    assertCurrent() {}
  }
  const db = IpcGattDatabase.fromPayload(manager, connection, {
    schemaVersion: 2,
    handle: 'db',
    databaseId: 'db-id',
    databaseGeneration: '1',
    services: [{ uuid: '180d', occurrence: '0', primary: true, includedServices: [] }],
    characteristics: [
      {
        handle: 'characteristic',
        serviceUuid: '180d',
        serviceOccurrence: '0',
        characteristicUuid: '2a39',
        characteristicOccurrence: '0',
        properties: ['write']
      }
    ],
    descriptors: []
  })
  const snapshot = await db.snapshot()
  return { db, path: snapshot.characteristics[0].path, manager }
}
const flush = async () => {
  for (let i = 0; i < 12; i++) await Promise.resolve()
}
const options = (signal = null, deadline = null) => ({
  signal,
  deadline,
  recording: { id: 'recording', maxBytes: 10000, maxRecords: 100 },
  delivery: { itemCapacity: 10, byteCapacity: 1000, reservedControlCapacity: 1, overflowPolicy: 'fail' }
})
const result = {
  kind: 'started',
  receipt: {
    version: 2,
    recordingId: 'recording',
    collectionId: '1',
    state: 'active',
    sourceGeneration: null,
    members: [
      { memberId: '0', acquisition: 'acquired', delivery: 'notification', cleanupReleased: false, cleanupError: null }
    ],
    setupFailure: null,
    fenceFailure: null,
    error: null,
    inspectionError: null,
    driverError: null
  }
}
const members = path => [{ memberId: '0', path, deliveryMode: 'prefer-notification' }]

describe('ordered recording startup controls', () => {
  beforeEach(() => jest.useFakeTimers())
  afterEach(() => jest.useRealTimers())
  test.each(['abort', 'deadline'])('%s bounds reservation; retry retains token and handle', async mode => {
    const reservation = deferred()
    const route = jest.fn(command =>
      command.endsWith('.reserve')
        ? reservation.promise
        : command.endsWith('.start')
          ? Promise.resolve(result)
          : Promise.resolve({ recoveryToken: 'recovery' })
    )
    const { db, path } = await fixture(route)
    const start = db.reserveOrderedRecordingStart(),
      controller = new AbortController()
    const first = start.start(
      members(path),
      options(controller.signal, mode === 'deadline' ? performance.now() + 10 : null)
    )
    const rejected = expect(first).rejects.toMatchObject({
      normalized: { code: mode === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
    })
    if (mode === 'abort') controller.abort()
    else jest.advanceTimersByTime(10)
    await rejected
    const retry = start.start(members(path), options())
    expect(route.mock.calls.filter(([c]) => c.endsWith('.reserve'))).toHaveLength(1)
    reservation.resolve({ startHandle: '7' })
    await expect(retry).resolves.toMatchObject({ kind: 'started' })
    expect(route.mock.calls.filter(([c]) => c.endsWith('.reserve'))).toHaveLength(1)
    expect(route).toHaveBeenCalledWith('gatt.ordered-recording.reserve', { reservationToken: 'lease-recording-0' })
    expect(route.mock.calls.filter(([c]) => c.endsWith('.start'))).toHaveLength(1)
    expect(start.recoveryHandle()).toEqual({ kind: 'ordered-recording-recovery' })
  })
  test('retirement during reservation prevents dispatch', async () => {
    const reservation = deferred()
    const route = jest.fn(c => (c.endsWith('.reserve') ? reservation.promise : Promise.resolve({})))
    const { db, path } = await fixture(route)
    const start = db.reserveOrderedRecordingStart()
    const pending = start.start(members(path), options())
    const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    const retirement = start.retire()
    reservation.resolve({ startHandle: '7' })
    await retirement
    await rejected
    expect(route.mock.calls.some(([c]) => c.endsWith('.start'))).toBe(false)
    await expect(start.status()).resolves.toEqual({ state: 'retired' })
  })
  test('pre-abort and pre-expiry issue no reservation', async () => {
    const route = jest.fn(),
      { db, path } = await fixture(route),
      start = db.reserveOrderedRecordingStart()
    const controller = new AbortController()
    controller.abort()
    await expect(start.start(members(path), options(controller.signal))).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    await expect(start.start(members(path), options(null, performance.now()))).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(route).not.toHaveBeenCalled()
  })
  test('monotonic expiry prevents dispatch before deadline timer runs', async () => {
    const reservation = deferred(),
      route = jest.fn(() => reservation.promise)
    const { db, path } = await fixture(route),
      start = db.reserveOrderedRecordingStart()
    const clock = jest.spyOn(db, 'monotonicNow').mockReturnValueOnce(0).mockReturnValueOnce(0).mockReturnValue(11)
    const pending = start.start(members(path), options(null, 10))
    reservation.resolve({ startHandle: '7' })
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(route.mock.calls.some(([c]) => c.endsWith('.start'))).toBe(false)
    clock.mockRestore()
  })
  test.each(['start', 'recovery'])('stalled %s is bounded and status recovers the result', async stage => {
    const wait = deferred()
    const route = jest.fn(c => {
      if (c.endsWith('.reserve')) return Promise.resolve({ startHandle: '7' })
      if (c.endsWith('.start')) return stage === 'start' ? wait.promise : Promise.resolve(result)
      if (c.endsWith('.recovery'))
        return stage === 'recovery' ? wait.promise : Promise.resolve({ recoveryToken: 'recovery' })
      if (c.endsWith('.status')) return Promise.resolve({ state: 'result', result })
      throw new Error(c)
    })
    const { db, path } = await fixture(route),
      start = db.reserveOrderedRecordingStart()
    const pending = start.start(members(path), options(null, performance.now() + 10))
    const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    await flush()
    jest.advanceTimersByTime(10)
    await rejected
    wait.resolve(stage === 'start' ? result : { recoveryToken: 'recovery' })
    await flush()
    await expect(start.status()).resolves.toMatchObject({ state: 'result', result: { kind: 'started' } })
    expect(start.recoveryHandle()).toEqual({ kind: 'ordered-recording-recovery' })
  })
})
