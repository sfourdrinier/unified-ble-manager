// __tests__/backends/reactnative/host-reset-wire.test.js
const { parseDrainValue } = require('../../../src/backends/reactnative/rust-core-wire')

it('preserves the host reset cause and platform detail', () => {
  const record = {
    t: 'link', ordinal: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'database',
    reason: 'backend-restart',
    platform: { domain: 'ubm', code: 'source-observation-overflow', message: 'Host observation capacity exceeded', metadata: {} }
  }
  expect(parseDrainValue({ more: false, records: [record], controlLost: 0 }, null))
    .toMatchObject({ ok: true, value: { records: [record] } })
})
