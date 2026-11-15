// __tests__/ipc/ordered-marker-startup.test.js
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
      rendererLease: { leaseId: 'lease' }
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
const admitted = {
  statusFormat: 'public',
  status: { state: 'admitted', ordinal: '7', write: { state: 'settled', error: null } }
}
const flush = async () => {
  for (let i = 0; i < 12; i++) await Promise.resolve()
}

describe.each(['gatt.ordered.marker', 'gatt.ordered-recording.marker'])('%s startup', prefix => {
  beforeEach(() => jest.useFakeTimers())
  afterEach(() => jest.useRealTimers())
  test('deadline bounds stalled reservation and late handle is cancelled without starting', async () => {
    const pending = deferred()
    const route = jest.fn(command => (command.endsWith('.reserve') ? pending.promise : Promise.resolve(admitted)))
    const { db, path } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, async () => ({ orderedHandle: 'group' }))
    const start = marker.start(path, new Uint8Array([1]), 'with-response', {
      signal: null,
      deadline: performance.now() + 10
    })
    const rejected = expect(start).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    await flush()
    jest.advanceTimersByTime(10)
    await rejected
    pending.resolve({ markerHandle: '7' })
    await flush()
    expect(route).toHaveBeenCalledWith(
      `${prefix}.reserve`,
      expect.objectContaining({ reservationToken: 'lease-marker-0' })
    )
    expect(route).toHaveBeenCalledWith(`${prefix}.cancel`, expect.objectContaining({ markerHandle: '7' }))
    await expect(marker.status()).resolves.toEqual(admitted.status)
    expect(route.mock.calls.some(([command]) => command.endsWith('.start'))).toBe(false)
  })
  test('owner wait is bounded and retry cannot start while cancellation is pending', async () => {
    const ownerWait = deferred(),
      cancelWait = deferred()
    let calls = 0
    const route = jest.fn(command =>
      command.endsWith('.reserve')
        ? Promise.resolve({ markerHandle: '7' })
        : command.endsWith('.cancel')
          ? cancelWait.promise
          : Promise.resolve(admitted)
    )
    const { db, path } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, () =>
      ++calls === 1 ? Promise.resolve({ orderedHandle: 'group' }) : ownerWait.promise
    )
    const first = marker.start(path, new Uint8Array([1]), 'with-response', {
      signal: null,
      deadline: performance.now() + 10
    })
    const timeout = expect(first).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    await flush()
    jest.advanceTimersByTime(10)
    await timeout
    const retry = marker.start(path, new Uint8Array([1]), 'with-response', { signal: null, deadline: null })
    ownerWait.resolve({ orderedHandle: 'group' })
    await flush()
    expect(route.mock.calls.some(([command]) => command.endsWith('.start'))).toBe(false)
    cancelWait.resolve(admitted)
    await expect(retry).resolves.toEqual(admitted.status)
    expect(route.mock.calls.filter(([command]) => command.endsWith('.cancel'))).toHaveLength(1)
  })
  test('abort while reserving is immediate and queued cancel errors remain visible', async () => {
    const reservation = deferred(),
      failure = new Error('cancel failed')
    const route = jest.fn(command => (command.endsWith('.reserve') ? reservation.promise : Promise.reject(failure)))
    const { db, path } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, async () => ({ orderedHandle: 'group' }))
    const controller = new AbortController()
    const start = marker.start(path, new Uint8Array([1]), 'with-response', {
      signal: controller.signal,
      deadline: null
    })
    const aborted = expect(start).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await flush()
    controller.abort()
    await aborted
    reservation.resolve({ markerHandle: '7' })
    await flush()
    await expect(marker.status()).rejects.toBe(failure)
  })
  test('successful forget takes precedence over an earlier queued cancel error', async () => {
    const reservation = deferred(),
      failure = new Error('cancel failed')
    const route = jest.fn(command => {
      if (command.endsWith('.reserve')) return reservation.promise
      if (command.endsWith('.cancel')) return Promise.reject(failure)
      if (command.endsWith('.forget')) return Promise.resolve({})
      throw new Error(`unexpected command: ${command}`)
    })
    const { db, path } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, async () => ({ orderedHandle: 'group' }))
    const controller = new AbortController()
    const start = marker.start(path, new Uint8Array([1]), 'with-response', {
      signal: controller.signal,
      deadline: null
    })
    const aborted = expect(start).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await flush()
    controller.abort()
    await aborted
    reservation.resolve({ markerHandle: '7' })
    await flush()
    await marker.forget()
    await expect(marker.status()).resolves.toEqual({ state: 'retired' })
    await expect(marker.cancel()).resolves.toEqual({ state: 'retired' })
  })
  test('explicit cancel settles a dispatched start whose reply is stalled', async () => {
    const dispatched = deferred(),
      startReply = deferred()
    const notAdmitted = { statusFormat: 'public', status: { state: 'not-admitted', error: null } }
    const route = jest.fn(command => {
      if (command.endsWith('.reserve')) return Promise.resolve({ markerHandle: '7' })
      if (command.endsWith('.start')) {
        dispatched.resolve()
        return startReply.promise
      }
      return Promise.resolve(notAdmitted)
    })
    const { db, path } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, async () => ({ orderedHandle: 'group' }))
    const start = marker.start(path, new Uint8Array([1]), 'with-response', { signal: null, deadline: null })
    await dispatched.promise
    const cancel = marker.cancel()
    await expect(cancel).resolves.toEqual(notAdmitted.status)
    await expect(start).resolves.toEqual(notAdmitted.status)
    startReply.resolve(admitted)
    await flush()
    await expect(start).resolves.toEqual(notAdmitted.status)
  })
  test('preexpired starts allocate nothing and successful starts clear their timer', async () => {
    const route = jest.fn(command =>
      command.endsWith('.reserve') ? Promise.resolve({ markerHandle: '7' }) : Promise.resolve(admitted)
    )
    const { db, path, manager } = await fixture(route)
    const marker = db.reserveMarkerWrite(prefix, async () => ({ orderedHandle: 'group' }))
    await expect(
      marker.start(path, new Uint8Array([1]), 'with-response', { signal: null, deadline: performance.now() })
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(route).not.toHaveBeenCalled()
    expect(manager.nextOrderedHandle).toBe(1)
    await expect(
      marker.start(path, new Uint8Array([1]), 'with-response', { signal: null, deadline: performance.now() + 10 })
    ).resolves.toEqual(admitted.status)
    expect(jest.getTimerCount()).toBe(0)
  })
})
