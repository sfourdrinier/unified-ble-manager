// __tests__/public/ordered-marker-controls.test.js
const { createPublicGattDatabase } = require('../../src/public/gatt')
const { createOrderedMarkerReservations } = require('../../src/core/ordered-marker-handle')

async function fixture(handle) {
  const path = {
    attachmentId: 'attachment',
    peerId: 'peer',
    connectionId: 'connection',
    ownerLeaseId: 'owner',
    connectionGeneration: '1',
    databaseId: 'database',
    databaseGeneration: '1'
  }
  const service = { ...path, serviceUuid: '180f', serviceOccurrence: '0' }
  const database = await createPublicGattDatabase({
    monotonicNow: () => performance.now(),
    snapshot: async () => ({
      path,
      services: [{ path: service, primary: true, includedServices: [] }],
      characteristics: [
        {
          path: { ...service, characteristicUuid: '2a19', characteristicOccurrence: '0', validity: 'current' },
          properties: {
            read: true,
            writeWithResponse: true,
            writeWithoutResponse: false,
            notify: false,
            indicate: false
          }
        }
      ],
      descriptors: []
    }),
    reserveOrderedRecordingMarker: () => handle
  })
  return {
    marker: database.reserveOrderedRecordingMarker('recording'),
    target: database.service('180f').characteristic('2a19')
  }
}

test.each(['deadline', 'abort'])('public marker retry bounds its pending status probe by %s', async control => {
  jest.useFakeTimers()
  try {
    let finishReservation
    const reservation = new Promise(resolve => {
      finishReservation = resolve
    })
    const actual = { state: 'not-admitted', error: null }
    const transport = {
      reserve: jest.fn(() => reservation),
      start: jest.fn(),
      cancel: jest.fn(async () => actual),
      status: jest.fn(async () => actual),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => performance.now(),
      () => 'same-key'
    )()
    const { marker, target } = await fixture(handle)
    const first = marker.start(target, Uint8Array.of(1), 'with-response', { timeoutMs: 10 })
    const firstTimeout = expect(first).rejects.toMatchObject({ code: 'operation.timed-out' })
    jest.advanceTimersByTime(10)
    await firstTimeout
    const controller = new AbortController()
    const retry = marker.start(
      target,
      Uint8Array.of(1),
      'with-response',
      control === 'deadline' ? { timeoutMs: 5 } : { signal: controller.signal }
    )
    const interrupted = expect(retry).rejects.toMatchObject({
      code: control === 'deadline' ? 'operation.timed-out' : 'operation.aborted'
    })
    if (control === 'deadline') jest.advanceTimersByTime(5)
    else controller.abort()
    await interrupted
    expect(transport.reserve.mock.calls).toEqual([['same-key']])
    expect(transport.start).not.toHaveBeenCalled()
    finishReservation('7')
    await expect(marker.status()).resolves.toEqual(actual)
    expect(transport.cancel).toHaveBeenCalledWith('7')
    await marker.forget()
  } finally {
    jest.useRealTimers()
  }
})

test('an already-aborted retry does not begin a status probe', async () => {
  const handle = {
    start: jest.fn(async () => ({ state: 'pending' })),
    status: jest.fn(async () => {
      throw new Error('status must not run')
    }),
    cancel: jest.fn(),
    forget: jest.fn()
  }
  const { marker, target } = await fixture(handle)
  await marker.start(target, Uint8Array.of(1), 'with-response')
  const controller = new AbortController()
  controller.abort()
  await expect(
    marker.start(target, Uint8Array.of(1), 'with-response', { signal: controller.signal })
  ).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(handle.status).not.toHaveBeenCalled()
  expect(handle.start).toHaveBeenCalledTimes(1)
})
