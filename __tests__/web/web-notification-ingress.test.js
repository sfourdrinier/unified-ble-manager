// __tests__/web/web-notification-ingress.test.js

const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')

describe('Web notification ingress', () => {
  test('stages alternating instance callbacks before activation and fences retirement', () => {
    const source = new WebNotificationIngress()
    const a = { uuid: 'same' }
    const b = { uuid: 'same' }
    const group = source.register(
      new Map([
        [a, 'a'],
        [b, 'b']
      ]),
      4,
      16
    )
    const bytes = new Uint8Array([1])
    source.admit(a, bytes)
    bytes[0] = 9
    source.admit(b, new Uint8Array([2]))
    group.retire('closed')
    source.admit(a, new Uint8Array([3]))
    expect(group.drain()).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1]) },
      { kind: 'value', memberId: 'b', ordinal: '2', value: new Uint8Array([2]) },
      { kind: 'terminal', reason: 'closed', uncertainTail: true }
    ])
    const next = source.register(new Map([[a, 'new']]), 1, 1)
    source.admit(a, new Uint8Array([4]))
    expect(next.drain()[0]).toMatchObject({ memberId: 'new', ordinal: '4' })
  })

  test('reserves control path on overflow and fences disconnect', () => {
    const source = new WebNotificationIngress()
    const a = {}
    const group = source.register(new Map([[a, 'a']]), 1, 4)
    source.admit(a, new Uint8Array([1]))
    source.admit(a, new Uint8Array([2, 3]))
    source.admit(a, new Uint8Array([4]))
    expect(group.drain()).toEqual([
      { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1]) },
      { kind: 'loss', lostValues: 1, lostBytes: 2 },
      { kind: 'terminal', reason: 'overflow', uncertainTail: true }
    ])
    const next = source.register(new Map([[a, 'a']]), 1, 1)
    source.retire('connection-lost')
    source.admit(a, new Uint8Array([5]))
    expect(next.drain()).toEqual([{ kind: 'terminal', reason: 'connection-lost', uncertainTail: true }])
    expect(() => source.register(new Map([[a, 'a']]), 1, 1)).toThrow()
  })

  test('retains a host reset as its own terminal cause', () => {
    const source = new WebNotificationIngress()
    const group = source.register(new Map([[{}, 'a']]), 1, 1)
    source.retire('host-reset')
    expect(group.drain()).toEqual([{ kind: 'terminal', reason: 'host-reset', uncertainTail: true }])
  })
})

test('marker shares source ordinal order and refuses retired groups', () => {
  const source = new WebNotificationIngress()
  const characteristic = {}
  const group = source.register(new Map([[characteristic, 'a']]), 4, 16)
  source.admit(characteristic, new Uint8Array([1]))
  const positions = []
  expect(
    group.admitMarker(ordinal => {
      positions.push(ordinal)
      return true
    })
  ).toBe('2')
  source.admit(characteristic, new Uint8Array([2]))
  expect(positions).toEqual(['2'])
  expect(
    group
      .drain()
      .filter(record => record.kind === 'value')
      .map(record => record.ordinal)
  ).toEqual(['1', '3'])
  group.retire('closed')
  expect(
    group.admitMarker(() => {
      throw new Error('retired marker admitted')
    })
  ).toBeNull()
})

test('a failed sink accounts for all earlier staged values before the later overflow', () => {
  const source = new WebNotificationIngress(); const characteristic = {}; const error = new Error('sink failed')
  const sink = jest.fn(() => { throw error })
  const group = source.register(new Map([[characteristic, 'a']]), 2, 16, sink)
  source.holdCallbacks()
  source.admit(characteristic, new Uint8Array([1]))
  source.admit(characteristic, new Uint8Array([2, 3]))
  source.admit(characteristic, new Uint8Array([4, 5, 6]))
  expect(() => source.releaseCallbacks()).toThrow(error)
  expect(sink).toHaveBeenCalledTimes(1)
  expect(group.drain()).toEqual([
    { kind: 'loss', reason: 'source-failed', lostValues: 1, lostBytes: 1 },
    { kind: 'loss', reason: 'source-failed', lostValues: 1, lostBytes: 2 },
    { kind: 'loss', lostValues: 1, lostBytes: 3 },
    { kind: 'terminal', reason: 'overflow', uncertainTail: true }
  ])
})

test('a reentrant callback flush cannot deliver terminal before staged sinks', () => {
  const source = new WebNotificationIngress(); const characteristic = {}; const order = []
  let group
  group = source.register(new Map([[characteristic, 'a']]), 2, 16, (_member, ordinal) => {
    order.push(`value-${ordinal}`)
    if (ordinal === '1') {
      source.admit(characteristic, new Uint8Array([2]))
      group.retire('closed')
      source.releaseCallbacks()
    }
    return true
  })
  group.onTerminal(() => order.push('terminal'))
  source.holdCallbacks()
  source.admit(characteristic, new Uint8Array([1]))
  expect(group.admitMarker(() => true)).toBeNull()
  expect(order).toEqual(['value-1', 'value-2', 'terminal'])
})


test.each(['throw', 'overflow'])('preserves %s sink and membership removal failures', mode => {
  const sinkFailure = new Error('sink failure'); const firstRemoval = new Error('first removal'); const secondRemoval = new Error('second removal')
  let removals = 0
  const source = new WebNotificationIngress((_member, added) => { if (!added) throw ++removals === 1 ? firstRemoval : secondRemoval })
  const characteristic = {}
  source.register(new Map([[characteristic, 'a']]), 2, 8, () => { if (mode === 'throw') throw sinkFailure; return false })
  let failure
  try { source.admit(characteristic, new Uint8Array([1])) } catch (error) { failure = error }
  const flatten = error => error instanceof AggregateError ? error.errors.flatMap(flatten) : [error]
  const failures = flatten(failure)
  if (mode === 'throw') expect(failures).toContain(sinkFailure)
  else expect(failures).toContain(firstRemoval)
  expect(failures).toContain(mode === 'throw' ? firstRemoval : secondRemoval)
})
