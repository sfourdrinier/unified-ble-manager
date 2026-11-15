// __tests__/ipc/ordered-delivery.test.js

const { OrderedDelivery } = require('../../src/ipc/ordered-delivery')

const tick = () => new Promise(resolve => setImmediate(resolve))

test('lost responses and acknowledgements retry the retained A/B/A FIFO', async () => {
  const source = (async function* () {
    yield { memberId: 'A', ordinal: '9007199254740993', value: Uint8Array.of(0, 255) }
    yield { memberId: 'B', ordinal: '18446744073709551614', value: Uint8Array.of(2) }
    yield { memberId: 'A', ordinal: '18446744073709551615', value: Uint8Array.of(3) }
  })()
  const delivery = new OrderedDelivery(source)
  expect(delivery.poll()).toEqual({ state: 'empty' })
  await tick()
  const first = delivery.poll()
  expect(first.item.ordinal).toBe('9007199254740993')
  expect(delivery.poll()).toEqual(first)
  expect(delivery.poll(first.deliveryId)).toEqual({ state: 'empty' })
  await tick()
  const second = delivery.poll(first.deliveryId)
  expect(second.item.memberId).toBe('B')
  expect(delivery.poll(first.deliveryId)).toEqual(second)
  expect(() => delivery.poll('999')).toThrow()
  expect(delivery.poll(second.deliveryId)).toEqual({ state: 'empty' })
  await tick()
  const third = delivery.poll(second.deliveryId)
  expect(third.item.ordinal).toBe('18446744073709551615')
  expect(third.item.memberId).toBe('A')
})

test('a source failure remains visible on every poll', async () => {
  const source = {
    next: async () => {
      throw new Error('source failed')
    }
  }
  const delivery = new OrderedDelivery(source)
  expect(delivery.poll()).toEqual({ state: 'empty' })
  await tick()
  expect(() => delivery.poll()).toThrow('source failed')
  expect(() => delivery.poll()).toThrow('source failed')
})

test('an oversized retained value becomes explicit loss followed by a terminal', async () => {
  const source = (async function* () {
    yield { kind: 'value', value: Uint8Array.of(1, 2) }
    yield { kind: 'value', value: Uint8Array.of(3) }
  })()
  const delivery = new OrderedDelivery(source)
  delivery.poll()
  await tick()
  const original = delivery.poll()
  delivery.replaceRetainedWithLoss(
    { kind: 'loss', lostValues: '1', lostBytes: '2' },
    [{ kind: 'terminal', uncertainTail: true }]
  )
  const loss = delivery.poll()
  expect(loss).toMatchObject({ deliveryId: original.deliveryId, item: { kind: 'loss', lostValues: '1' } })
  expect(delivery.poll()).toEqual(loss)
  const terminal = delivery.poll(loss.deliveryId)
  expect(terminal).toMatchObject({ item: { kind: 'terminal', uncertainTail: true } })
  expect(delivery.poll(terminal.deliveryId)).toEqual({ state: 'closed' })
})
