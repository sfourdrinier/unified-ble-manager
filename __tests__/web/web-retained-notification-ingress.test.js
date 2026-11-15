const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')

test('retains copied deliveries until their exact head is acknowledged', () => {
  const source = new WebNotificationIngress()
  const a = {}
  const b = {}
  const group = source.registerRetained(
    new Map([
      [a, 'a'],
      [b, 'b']
    ]),
    2,
    8
  )
  const value = new Uint8Array([1, 2])
  source.admit(a, value)
  value[0] = 9
  source.admit(b, new Uint8Array())
  const first = group.peek()
  expect(first).toEqual({
    id: '1',
    record: { kind: 'value', memberId: 'a', ordinal: '1', value: new Uint8Array([1, 2]) }
  })
  first.record.value[0] = 7
  expect(group.peek().record.value).toEqual(new Uint8Array([1, 2]))
  expect(group.ackExact('2')).toBe(false)
  expect(group.peek().id).toBe('1')
  expect(group.ackExact('1')).toBe(true)
  expect(group.ackExact('1')).toBe(false)
  expect(group.peek()).toEqual({
    id: '2',
    record: { kind: 'value', memberId: 'b', ordinal: '2', value: new Uint8Array() }
  })
})

test('overflow fences admission and retains known loss and uncertain terminal behind the head', () => {
  const source = new WebNotificationIngress()
  const a = {}
  const group = source.registerRetained(new Map([[a, 'a']]), 1, 2)
  source.admit(a, new Uint8Array([1]))
  source.admit(a, new Uint8Array([2, 3]))
  source.admit(a, new Uint8Array([4]))
  expect(group.terminal).toBe(true)
  expect(group.peek().id).toBe('1')
  expect(group.ackExact('1')).toBe(true)
  expect(group.peek()).toEqual({ id: '2', record: { kind: 'loss', lostValues: 1, lostBytes: 2 } })
  expect(group.ackExact('2')).toBe(true)
  expect(group.peek()).toEqual({ id: '3', record: { kind: 'terminal', reason: 'overflow', uncertainTail: true } })
  expect(group.ackExact('3')).toBe(true)
  expect(group.peek()).toBeNull()
})
