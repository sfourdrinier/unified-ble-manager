const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { AggregateStreamQuota } = require('../../src/core/aggregate-stream-quota')

test('cache release remains possible if a separately retained view exceeds the quota', () => {
  const quota = new AggregateStreamQuota(100)
  const source = new CoreBoundedStream({ itemCapacity: 2, byteCapacity: 200, reservedControlCapacity: 0 }, 'error', true)
  quota.register(source)
  const admit = source.createRetainedByteAdmission()
  expect(admit(40)).toBe(true)
  let viewBytes = 50
  source.trackRetainedView({ retainedBytes: () => viewBytes, onRetainedValuesDrained: () => {} })
  // Model an independently growing view after its initial admission. New
  // views that already exceed the quota are refused at registration.
  viewBytes = 120
  expect(quota.retainedBytes()).toBeGreaterThan(100)
  expect(admit(0)).toBe(true)
  expect(quota.retainedBytes()).toBe(120)
})
