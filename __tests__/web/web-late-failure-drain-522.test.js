const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')

test('a retained v2 source keeps observing callbacks after overflow until cleanup fences it', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{
    id: 'heart', characteristic, lease: { enable: async () => {}, release: async () => {} }
  }], 1, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  ingress.admit(characteristic, Uint8Array.of(1))
  ingress.admit(characteristic, Uint8Array.of(2, 3))
  expect(source.peek().record.kind).toBe('value')
  expect(source.ackExact(source.peek().id)).toBe(true)
  expect(source.peek().record).toMatchObject({ kind: 'failure-observation', sourceRejection: { bytes: '2' } })
  expect(source.ackExact(source.peek().id)).toBe(true)
  ingress.admit(characteristic, Uint8Array.of(4, 5, 6))
  expect(source.peek().record).toMatchObject({ kind: 'failure-observation', sourceRejection: { bytes: '3' } })
})

test('bounded failures retain exact head and reserve an honest omission before sealing', () => {
  const ingress = new WebNotificationIngress()
  const a = {}, b = {}
  const owner = new WebRecordingSourceOwner(ingress, [
    { id: 'a', characteristic: a, lease: { enable: async () => {}, release: async () => {} } },
    { id: 'b', characteristic: b, lease: { enable: async () => {}, release: async () => {} } }
  ], 1, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  ingress.admit(a, Uint8Array.of(1))
  ingress.admit(a, Uint8Array.of(2, 3))
  ingress.admit(b, Uint8Array.of(4, 5, 6))
  ingress.admit(a, Uint8Array.of(7))
  expect(source.ackExact('1')).toBe(true)
  const first = source.peek()
  expect(first).toMatchObject({ id: '2', record: { kind: 'failure-observation', sourceRejection: { bytes: '2' }, sourceFailureMember: { member: '0' } } })
  expect(source.ackExact('3')).toBe(false)
  expect(source.peek()).toEqual(first)
  expect(source.ackExact('2')).toBe(true)
  expect(source.peek()).toEqual({ id: '3', record: { kind: 'failure-observations-omitted', count: null } })
  expect(source.ackExact('3')).toBe(true)
  ingress.admit(b, Uint8Array.of(8, 9, 10, 11))
  expect(source.peek()).toMatchObject({ id: '4', record: { kind: 'failure-observation', sourceRejection: { bytes: '4' }, sourceFailureMember: { member: '1' } } })
})

test('ordinal exhaustion continues reporting callbacks until the source is sealed', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'a', characteristic,
    lease: { enable: async () => {}, release: async () => {} } }], 2, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  ingress.ordinal = 18446744073709551615n
  ingress.admit(characteristic, Uint8Array.of(1))
  ingress.admit(characteristic, Uint8Array.of(2, 3))
  expect(source.peek().record.sourceRejection).toEqual({ kind: 'ordinal-exhausted', bytes: '1' })
  source.ackExact(source.peek().id)
  expect(source.peek().record.sourceRejection).toEqual({ kind: 'ordinal-exhausted', bytes: '2' })
  source.ackExact(source.peek().id)
  source.seal()
  expect(source.peek().record.kind).toBe('terminal')
})

test('delivery identity exhaustion reserves an omission and terminal without overflowing identities', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'a', characteristic,
    lease: { enable: async () => {}, release: async () => {} } }], 1, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  ;[...ingress.groups][0].nextDeliveryId = 18446744073709551614n
  ingress.admit(characteristic, Uint8Array.of(1))
  ingress.admit(characteristic, Uint8Array.of(2, 3))
  expect(source.peek()).toBeNull()
  source.seal()
  expect(source.peek()).toEqual({ id: '18446744073709551614', record: { kind: 'failure-observations-omitted', count: null } })
  expect(source.ackExact(source.peek().id)).toBe(true)
  expect(source.peek()).toMatchObject({ id: '18446744073709551615', record: { kind: 'terminal' } })
})

test('release cannot discard a terminal created by sealing', async () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'a', characteristic,
    lease: { enable: async () => {}, release: async () => {} } }], 1, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  source.retire('closed')
  await expect(owner.release()).rejects.toThrow('still owns deliveries')
  expect(source.peek().record.kind).toBe('terminal')
  source.ackExact(source.peek().id)
  await expect(owner.release()).resolves.toBeUndefined()
})
