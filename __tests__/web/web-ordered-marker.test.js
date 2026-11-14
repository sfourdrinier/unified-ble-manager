// __tests__/web/web-ordered-marker.test.js
const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebOrderedMarkerTransactions } = require('../../src/web/web-ordered-marker')

function fixture() {
  const source = new WebNotificationIngress()
  const target = {
    writeValueWithResponse: jest.fn(() => Promise.resolve()),
    writeValueWithoutResponse: jest.fn(() => Promise.resolve())
  }
  const group = source.register(new Map([[target, 'member']]), 8, 64)
  const records = []
  const credit = {
    reserveMarker: jest.fn(),
    admitReservedMarker: jest.fn(marker => {
      records.push(marker)
      return true
    }),
    releaseMarkerNotAdmitted: jest.fn(),
    markerAdmissionStatus: jest.fn(() => ({ state: 'unknown' }))
  }
  const transactions = new WebOrderedMarkerTransactions(group, credit, 'c', '1', '1')
  return { source, target, group, records, credit, transactions }
}

test('marker precedes synchronous write and exact retry never writes twice', async () => {
  const { source, target, group, records, transactions } = fixture()
  source.admit(target, new Uint8Array([1]))
  target.writeValueWithResponse.mockImplementation(() => {
    expect(records[0]).toMatchObject({ markerId: '0', ordinal: '2' })
    source.admit(target, new Uint8Array([2]))
    return Promise.resolve()
  })
  expect(transactions.write('0', target, new Uint8Array([7]), 'with-response', null)).toMatchObject({
    marker: { state: 'admitted', ordinal: '2' },
    write: { state: 'started' }
  })
  expect(transactions.write('0', target, new Uint8Array([7]), 'with-response', null).write.state).toBe('started')
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
  expect(group.drain().map(record => record.ordinal)).toEqual(['1', '3'])
  expect(transactions.forget('0')).toBe(false)
  await Promise.resolve()
  transactions.acknowledge('0')
  expect(transactions.forget('0')).toBe(true)
  expect(transactions.forget('0')).toBe(true)
})

test('credit refusal and pre-abort never write', () => {
  const first = fixture()
  first.credit.reserveMarker.mockImplementation(() => {
    throw new Error('no credit')
  })
  expect(first.transactions.write('0', first.target, new Uint8Array(), 'with-response', null).marker.state).toBe(
    'not-admitted'
  )
  expect(first.target.writeValueWithResponse).not.toHaveBeenCalled()
  const second = fixture()
  const controller = new AbortController()
  controller.abort()
  expect(
    second.transactions.write('0', second.target, new Uint8Array(), 'with-response', controller.signal).marker.state
  ).toBe('not-admitted')
  expect(second.credit.reserveMarker).not.toHaveBeenCalled()
  expect(second.target.writeValueWithResponse).not.toHaveBeenCalled()
})

test('write rejection retains marker and settled error', async () => {
  const { target, transactions } = fixture()
  const failure = new Error('write failure')
  target.writeValueWithoutResponse.mockRejectedValue(failure)
  transactions.write('0', target, new Uint8Array([3]), 'without-response', null)
  await Promise.resolve()
  expect(transactions.write('0', target, new Uint8Array([3]), 'without-response', null)).toEqual({
    marker: { state: 'admitted', ordinal: '1' },
    write: { state: 'settled', result: { ok: false, error: failure } }
  })
})

test('a synchronous write callback retry cannot dispatch twice', () => {
  const { target, transactions } = fixture()
  target.writeValueWithResponse.mockImplementation(() => {
    expect(transactions.write('0', target, new Uint8Array([1]), 'with-response', null).write.state).toBe('started')
    return Promise.resolve()
  })
  transactions.write('0', target, new Uint8Array([1]), 'with-response', null)
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
})

test('a rejected marker preserves its refusal and does not dispatch', () => {
  const { target, credit, transactions } = fixture()
  credit.admitReservedMarker.mockReturnValue(false)
  expect(transactions.write('0', target, new Uint8Array([1]), 'with-response', null).marker.state).toBe('rejected')
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
  expect(credit.releaseMarkerNotAdmitted).not.toHaveBeenCalled()
  expect(transactions.forget('0')).toBe(false)
})

test('retained ingress rejection with no marker delivery can be forgotten', () => {
  const { target, group, credit } = fixture()
  credit.admitReservedMarker.mockReturnValue(false)
  const transactions = new WebOrderedMarkerTransactions(group, credit, 'c', '1', '1', null, 'no-rejection-delivery')
  expect(transactions.write('0', target, new Uint8Array([1]), 'with-response', null).marker.state).toBe('rejected')
  expect(transactions.forget('0')).toBe(true)
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
})

test('cancellation after marker admission retains marker and does not write', () => {
  const { target, credit, transactions } = fixture()
  const controller = new AbortController()
  credit.admitReservedMarker.mockImplementation(() => {
    controller.abort()
    return true
  })
  const receipt = transactions.write('0', target, new Uint8Array([1]), 'with-response', controller.signal)
  expect(receipt).toMatchObject({ marker: { state: 'admitted', ordinal: '1' }, write: { state: 'not-started' } })
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
  expect(transactions.forget('0')).toBe(false)
  transactions.acknowledge('0')
  expect(transactions.forget('0')).toBe(true)
})

test('receipt identity is exact and transaction IDs are contiguous', async () => {
  const { target, transactions } = fixture()
  transactions.write('0', target, new Uint8Array([1]), 'with-response', null)
  expect(() => transactions.write('0', target, new Uint8Array([2]), 'with-response', null)).toThrow()
  expect(() => transactions.write('1', target, new Uint8Array([1]), 'with-response', null)).toThrow()
  await Promise.resolve()
  transactions.acknowledge('0')
  expect(transactions.forget('0')).toBe(true)
  expect(() => transactions.write('2', target, new Uint8Array([1]), 'with-response', null)).toThrow()
  expect(transactions.write('1', target, new Uint8Array([1]), 'with-response', null).marker.state).toBe('admitted')
  expect(transactions.forget('0')).toBe(true)
})

test('synchronous browser failure keeps admitted marker and is never retried', () => {
  const { target, transactions } = fixture()
  const failure = new Error('synchronous failure')
  target.writeValueWithResponse.mockImplementation(() => {
    throw failure
  })
  const first = transactions.write('0', target, new Uint8Array([1]), 'with-response', null)
  expect(first).toEqual({
    marker: { state: 'admitted', ordinal: '1' },
    write: { state: 'settled', result: { ok: false, error: failure } }
  })
  expect(transactions.write('0', target, new Uint8Array([1]), 'with-response', null)).toEqual(first)
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
})

test('uncertain credit remains retained and retry reconciles the admitted marker without writing', () => {
  const { target, credit, transactions } = fixture()
  const failure = new Error('credit unknown')
  credit.admitReservedMarker.mockImplementation(() => {
    throw failure
  })
  expect(() => transactions.write('0', target, new Uint8Array(), 'with-response', null)).toThrow(failure)
  expect(transactions.forget('0')).toBe(false)
  expect(() => transactions.acknowledge('0')).toThrow(failure)
  credit.markerAdmissionStatus.mockReturnValue({ state: 'admitted', ordinal: '1' })
  expect(transactions.write('0', target, new Uint8Array(), 'with-response', null)).toMatchObject({
    marker: { state: 'admitted', ordinal: '1' },
    write: { state: 'not-started' }
  })
  expect(transactions.forget('0')).toBe(false)
  transactions.acknowledge('0')
  expect(transactions.forget('0')).toBe(true)
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
})
