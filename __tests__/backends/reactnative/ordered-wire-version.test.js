// __tests__/backends/reactnative/ordered-wire-version.test.js

const { parseOpValue } = require('../../../src/backends/reactnative/rust-core-wire')

test('ordered wire version probe accepts only a bounded integer in an exact envelope', () => {
  expect(parseOpValue('gatt.ordered.version', { wireVersion: 1 })).toEqual({
    ok: true,
    value: { wireVersion: 1 }
  })
  expect(parseOpValue('gatt.ordered.version', { wireVersion: '1' }).ok).toBe(false)
  expect(parseOpValue('gatt.ordered.version', { wireVersion: 1, extra: true }).ok).toBe(false)
})

test('ordered session results preserve unsigned IDs and reject malformed receipts', () => {
  const reserve = parseOpValue('gatt.ordered.reserve', { collectionId: '18446744073709551615', reclaimCapability: 'opaque' })
  expect(reserve.ok).toBe(true)
  expect(reserve.value.collectionId).toBe('18446744073709551615')
  expect(parseOpValue('gatt.ordered.reserve', { collectionId: '18446744073709551616', reclaimCapability: 'opaque' }).ok).toBe(false)
  expect(parseOpValue('gatt.ordered.poll', { deliveryId: '01', record: '{}' }).ok).toBe(false)
  expect(parseOpValue('gatt.ordered.discard', { receiptId: '2', discardedDeliveryId: '1', evidence: '{}' }).ok).toBe(true)
  expect(parseOpValue('gatt.ordered.ack-delivery', {}).ok).toBe(false)
})
