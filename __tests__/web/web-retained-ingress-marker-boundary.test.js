const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebOrderedMarkerTransactions } = require('../../src/web/web-ordered-marker')

function fixture(capacity = 2, wake) {
  const source = new WebNotificationIngress()
  const target = { writeValueWithResponse: jest.fn(() => Promise.resolve()) }
  const group = source.registerRetained(new Map([[target, 'a']]), capacity, 16, { membership: true, wake })
  const transactions = new WebOrderedMarkerTransactions(group, group, 'collection', 'connection', 'database')
  return { source, target, group, transactions }
}

test('reserved marker is retained in source order before the write, with exact acknowledgement and retry', async () => {
  const wake = jest.fn()
  const { source, target, group, transactions } = fixture(2, wake)
  source.admit(target, new Uint8Array([1]))
  target.writeValueWithResponse.mockImplementation(() => {
    source.admit(target, new Uint8Array([2]))
    return Promise.resolve()
  })
  const result = transactions.write('0', target, new Uint8Array([9]), 'with-response', null)
  expect(result).toMatchObject({ marker: { state: 'admitted', ordinal: '2' }, write: { state: 'started' } })
  expect(transactions.write('0', target, new Uint8Array([9]), 'with-response', null)).toEqual(result)
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
  expect(wake).toHaveBeenCalledTimes(3)
  expect(group.peek().record).toMatchObject({
    kind: 'value',
    ordinal: '1',
    sourceMembership: { member: 'a', epoch: '1' },
    provenance: 'read-or-notification'
  })
  expect(group.ackExact('2')).toBe(false)
  expect(group.ackExact('1')).toBe(true)
  const marker = group.peek()
  expect(marker).toEqual({
    id: '2',
    record: {
      kind: 'marker',
      collectionId: 'collection',
      connectionGeneration: 'connection',
      databaseGeneration: 'database',
      sourceGeneration: null,
      sourceMembership: null,
      markerId: '0',
      ordinal: '2'
    }
  })
  expect(group.peek()).toEqual(marker)
  expect(group.ackExact('2')).toBe(true)
  expect(group.peek().record).toMatchObject({
    kind: 'value',
    ordinal: '3',
    sourceMembership: { member: 'a', epoch: '1' }
  })
  await Promise.resolve()
})

test('reserved control credit works with full value capacity and refuses a second marker before write', () => {
  const { source, target, group, transactions } = fixture(1)
  source.admit(target, new Uint8Array([1]))
  expect(transactions.write('0', target, new Uint8Array([3]), 'with-response', null).marker.state).toBe('admitted')
  expect(() => group.reserveMarker('collection', '1')).toThrow()
  expect(target.writeValueWithResponse).toHaveBeenCalledTimes(1)
  expect(group.ackExact('1')).toBe(true)
  expect(group.peek().record.kind).toBe('marker')
  expect(group.ackExact('2')).toBe(true)
  expect(() => group.reserveMarker('collection', '1')).not.toThrow()
  group.releaseMarkerNotAdmitted('collection', '1')
})

test('pre-abort and marker credit refusal issue no browser write', () => {
  const { target, group, transactions } = fixture()
  group.reserveMarker('other', '0')
  expect(transactions.write('0', target, new Uint8Array(), 'with-response', null).marker.state).toBe('not-admitted')
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
  const next = fixture()
  const controller = new AbortController()
  controller.abort()
  expect(
    next.transactions.write('0', next.target, new Uint8Array(), 'with-response', controller.signal).marker.state
  ).toBe('not-admitted')
  expect(next.target.writeValueWithResponse).not.toHaveBeenCalled()
})

test('cancellation from marker wake retains the marker and prevents the write', () => {
  const controller = new AbortController()
  const { target, group, transactions } = fixture(2, () => controller.abort())
  const result = transactions.write('0', target, new Uint8Array([4]), 'with-response', controller.signal)
  expect(result).toMatchObject({ marker: { state: 'admitted', ordinal: '1' }, write: { state: 'not-started' } })
  expect(target.writeValueWithResponse).not.toHaveBeenCalled()
  expect(group.peek()).toMatchObject({ id: '1', record: { kind: 'marker', markerId: '0', ordinal: '1' } })
  expect(transactions.write('0', target, new Uint8Array([4]), 'with-response', controller.signal)).toEqual(result)
})

test('a failed journal commit retries the identical marker head before acknowledgement', async () => {
  const { WebRetainedRecordingPump } = require('../../src/core/web-retained-recording-pump')
  const { target, group, transactions } = fixture()
  transactions.write('0', target, new Uint8Array(), 'with-response', null)
  const commit = jest.fn().mockRejectedValueOnce(new Error('storage unavailable')).mockResolvedValue(undefined)
  const pump = new WebRetainedRecordingPump(group, { commit, retire: jest.fn().mockResolvedValue(undefined) })
  const head = group.peek()
  await expect(pump.runOnce()).rejects.toThrow('storage unavailable')
  expect(group.peek()).toEqual(head)
  await expect(pump.runOnce()).resolves.toBe('committed')
  expect(commit.mock.calls).toEqual([[head], [head]])
  expect(group.peek()).toBeNull()
})

test('a reentrant callback cannot insert a marker behind a later source ordinal', () => {
  const { source, target, group } = fixture()
  group.reserveMarker('collection', '0')
  expect(
    group.admitMarker(ordinal => {
      source.admit(target, new Uint8Array([1]))
      return group.admitReservedMarker({
        kind: 'marker',
        collectionId: 'collection',
        markerId: '0',
        ordinal,
        connectionGeneration: 'connection',
        databaseGeneration: 'database'
      })
    })
  ).toBeNull()
  expect(group.peek()).toMatchObject({ record: { kind: 'value', ordinal: '2' } })
  expect(group.markerAdmissionStatus('collection', '0')).toEqual({ state: 'rejected' })
})

test('ordinal exhaustion retains known rejected callback bytes only for matching groups', () => {
  const source = new WebNotificationIngress()
  const a = {},
    b = {}
  const matching = source.registerRetained(new Map([[a, 'a']]), 2, 8)
  const other = source.registerRetained(new Map([[b, 'b']]), 2, 8)
  source.ordinal = 18446744073709551615n
  source.admit(a, new Uint8Array([1, 2, 3]))
  expect(matching.peek()).toEqual({ id: '1', record: { kind: 'loss', lostValues: 1, lostBytes: 3 } })
  expect(matching.ackExact('1')).toBe(true)
  expect(matching.peek()).toEqual({
    id: '2',
    record: { kind: 'terminal', reason: 'ordinal-exhausted', uncertainTail: true }
  })
  expect(other.peek()).toEqual({
    id: '1',
    record: { kind: 'terminal', reason: 'ordinal-exhausted', uncertainTail: true }
  })
})
