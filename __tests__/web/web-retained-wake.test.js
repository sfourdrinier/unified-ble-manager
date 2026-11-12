const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')

test('retained admission wakes synchronously after each complete value and control', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const wake = jest.fn()
  const source = ingress.registerRetained(new Map([[characteristic, '0']]), 1, 8, { wake, membership: true })
  ingress.admit(characteristic, new Uint8Array([1]))
  expect(wake).toHaveBeenCalledTimes(1)
  expect(source.peek().record).toMatchObject({ sourceMembership: { member: '0', epoch: '1' }, delivery: 'unknown', provenance: 'read-or-notification' })
  ingress.admit(characteristic, new Uint8Array([2]))
  expect(wake).toHaveBeenCalledTimes(3)
  expect(source.terminalReason).toBe('overflow')
})

test('reentrant acknowledgement during wake does not leave phantom bytes', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  let source
  source = ingress.registerRetained(new Map([[characteristic, '0']]), 1, 1, { wake: () => source.ackExact(source.peek().id) })
  ingress.admit(characteristic, new Uint8Array([1]))
  ingress.admit(characteristic, new Uint8Array([2]))
  expect(source.terminal).toBe(false)
})

test('overflow fences before waking about the rejected value', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  let source, calls = 0
  source = ingress.registerRetained(new Map([[characteristic, '0']]), 1, 8, { wake: () => {
    calls++
    if (calls === 2) {
      source.ackExact('1')
      ingress.admit(characteristic, new Uint8Array([9]))
    }
  } })
  ingress.admit(characteristic, new Uint8Array([1]))
  ingress.admit(characteristic, new Uint8Array([2]))
  expect(source.terminalReason).toBe('overflow')
  expect(source.peek().record.kind).toBe('loss')
  source.ackExact('2')
  expect(source.peek().record.kind).toBe('terminal')
})

test('reentrant wakes cannot invert global ordinals in another retained group', () => {
  const ingress = new WebNotificationIngress()
  const c = {}
  let nested = false
  ingress.registerRetained(new Map([[c, 'a']]), 4, 8, { wake: () => {
    if (!nested) { nested = true; ingress.admit(c, new Uint8Array([2])) }
  } })
  const second = ingress.registerRetained(new Map([[c, 'b']]), 4, 8)
  ingress.admit(c, new Uint8Array([1]))
  expect(second.peek().record.ordinal).toBe('1')
  second.ackExact('1')
  expect(second.peek().record.ordinal).toBe('2')
})
