// __tests__/web/web-notification-ingress.test.js

const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')

describe('Web notification ingress', () => {
  test('stages alternating instance callbacks before activation and fences retirement', () => {
    const source = new WebNotificationIngress()
    const a = { uuid: 'same' }
    const b = { uuid: 'same' }
    const group = source.register(new Map([[a, 'a'], [b, 'b']]), 4, 16)
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
})
