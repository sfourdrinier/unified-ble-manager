const wire = require('../../../src/backends/reactnative/rust-core-wire')

test('disconnect cleanup wire accepts separate mapped failures in order', () => {
  const failures = [
    { resourceKind: 'connection', code: 'platform.failure', domain: 'platform',
      operation: 'connection.disconnect', detail: 'platform: close failed',
      platform: { domain: 'android', code: 'platform', message: 'close failed', metadata: {} } },
    { resourceKind: 'connection', code: 'permission.denied', domain: 'adapter',
      operation: 'connection.disconnect', detail: 'permission-denied: receiver failed', platform: null }
  ]
  const parsed = wire.parseOpValue('connection.disconnect', { state: 'release-failed', failures })
  expect(parsed.ok).toBe(true)
  expect(parsed.value.failures).toEqual(failures)
})
