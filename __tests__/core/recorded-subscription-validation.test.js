// __tests__/core/recorded-subscription-validation.test.js

const { SubscriptionRegistry } = require('../../src/core/subscription-registry')

const valid = { id: 'session-1', maxBytes: 1048576, maxRecords: 1 }
const registry = new SubscriptionRegistry({ backend: { gatt: {} } })

test('recorded subscriptions reject invalid identities and quotas with their own error context', async () => {
  for (const recording of [
    { ...valid, id: '../escape' },
    { ...valid, id: 'x'.repeat(65) },
    { ...valid, maxBytes: 1048575 },
    { ...valid, maxBytes: 1073741825 },
    { ...valid, maxRecords: 0 },
    { ...valid, maxRecords: 1000001 },
    { ...valid, maxRecords: 1.5 }
  ]) {
    await expect(registry.subscribeRecorded({}, { recording }, 'queue')).rejects.toMatchObject({
      normalized: { code: 'argument.invalid', domain: 'gatt', operation: 'subscription-registry.recording' }
    })
  }
})

test('both quota boundaries reach the host capability check', async () => {
  for (const recording of [valid, { ...valid, maxBytes: 1073741824, maxRecords: 1000000 }]) {
    await expect(registry.subscribeRecorded({}, { recording }, 'queue')).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported', domain: 'gatt' }
    })
  }
})
