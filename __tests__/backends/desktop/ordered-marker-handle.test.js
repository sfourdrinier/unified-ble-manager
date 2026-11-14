// __tests__/backends/desktop/ordered-marker-handle.test.js
const { createOrderedMarkerReservations } = require('../../../src/core/ordered-marker-handle')
const controls = { signal: null, deadline: null }
test('lost reservation response retries same key and never duplicates the write', async () => {
  const keys = []
  let attempts = 0
  const transport = {
    reserve: jest.fn(async key => { keys.push(key); if (++attempts === 1) throw new Error('lost reserve response'); return '0' }),
    start: jest.fn(async () => ({ state: 'pending' })),
    status: jest.fn(async () => ({ state: 'admitted', ordinal: '4', write: { state: 'settled', error: null } })),
    cancel: jest.fn(async () => ({ state: 'not-admitted', error: null })),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(transport, target => target, () => 0, () => 'unique-key')()
  const target = {}
  await expect(handle.start(target, new Uint8Array([1]), 'with-response', controls)).rejects.toThrow('lost reserve response')
  expect(await handle.start(target, new Uint8Array([1]), 'with-response', controls)).toEqual({ state: 'pending' })
  expect(keys).toEqual(['unique-key', 'unique-key'])
  expect(transport.start).toHaveBeenCalledTimes(1)
  expect((await handle.status()).state).toBe('admitted')
  await handle.forget()
  await expect(handle.start(target, new Uint8Array([1]), 'with-response', controls)).rejects.toThrow('lifecycle.invalid-state')
})
test('idle cancellation consumes no native ID and is retained through retry', async () => {
  const transport = { reserve: jest.fn(), start: jest.fn(), status: jest.fn(), cancel: jest.fn(), forget: jest.fn() }
  const handle = createOrderedMarkerReservations(transport, target => target, () => 0, () => 'key')()
  expect((await handle.cancel()).state).toBe('not-admitted')
  expect((await handle.start({}, new Uint8Array([1]), 'with-response', controls)).state).toBe('not-admitted')
  await handle.forget()
  expect(transport.reserve).not.toHaveBeenCalled()
  expect(transport.start).not.toHaveBeenCalled()
})
test('abort while reservation response is pending cancels native ownership before start', async () => {
  let finishReserve
  const reserved = new Promise(resolve => { finishReserve = resolve })
  const transport = {
    reserve: jest.fn(() => reserved),
    start: jest.fn(async () => ({state:'pending'})),
    status: jest.fn(async () => ({state:'not-admitted',error:null})),
    cancel: jest.fn(async () => ({state:'not-admitted',error:null})),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(transport, target => target, () => 0, () => 'reservation-key')()
  const abort = new AbortController()
  const pending = handle.start({}, new Uint8Array([3]), 'with-response', {signal:abort.signal, deadline:null})
  abort.abort()
  finishReserve('0')
  expect(await pending).toEqual({state:'not-admitted',error:null})
  expect(transport.cancel).toHaveBeenCalledTimes(1)
  expect(transport.start).not.toHaveBeenCalled()
  await handle.forget()
})
