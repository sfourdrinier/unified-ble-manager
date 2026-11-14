// __tests__/backends/reactnative/notification-source-failure-wire.test.js

const { parseDrainValue } = require('../../../src/backends/reactnative/rust-core-wire')
const { aggregateContinuationClaim } = require('../../../src/backends/reactnative/react-native-continuation-claim')
const { normalizeBackgroundContinuation } = require('../../../src/backend-contract/background-continuation')

const failure = {
  code: 'platform.failure',
  domain: 'platform',
  operation: 'gatt.subscribe',
  detail: null,
  platform: { domain: 'android', code: 'cccd-failed', message: 'Radio failure', metadata: { status: 133 } }
}
const end = {
  t: 'stream-end',
  ordinal: 1,
  consumer: 'ubm-continuation-0',
  reason: 'source-failed',
  droppedItems: 0,
  droppedBytes: 0
}
const evidence = {
  producer: 'subscription',
  kind: 'committed',
  ref: 'native-stream-end-evidence',
  byteLength: 19,
  digest: 'a'.repeat(64),
  formatVersion: 1,
  durability: 'committed',
  scope: { managerId: 'native-manager', operationId: 'native-operation', incarnation: '1' }
}

it('validates optional structured stream errors without accepting them for other reasons', () => {
  const parse = record => parseDrainValue({ more: false, records: [record], controlLost: 0 }, null)
  expect(parse({ ...end, error: failure })).toMatchObject({ ok: true, value: { records: [{ error: failure }] } })
  expect(parse(end)).toMatchObject({ ok: true, value: { records: [end] } })
  expect(parse({ ...end, error: null })).toMatchObject({ ok: false })
  expect(parse({ ...end, reason: 'closed', error: failure })).toMatchObject({ ok: false })
  expect(parse({ ...end, evidence })).toMatchObject({ ok: true, value: { records: [{ evidence }] } })
  expect(parse({ ...end, reason: 'invalidated', evidence })).toMatchObject({
    ok: true,
    value: { records: [{ evidence }] }
  })
  expect(parse({ ...end, evidence: { ...evidence, producer: 'scan' } })).toMatchObject({ ok: false })
  expect(parse({ ...end, error: { ...failure, code: 'invented' } })).toMatchObject({ ok: false })
})

it('retains the structured error in the continuation backlog', () => {
  const declaration = normalizeBackgroundContinuation({
    onAppearance: 'native',
    resubscribe: [
      {
        serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
        characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb'
      }
    ]
  })
  const claim = aggregateContinuationClaim(
    {
      consumerCount: 1,
      selectors: declaration.resubscribe,
      batches: [JSON.stringify({ more: false, records: [{ ...end, error: failure }], controlLost: 0 })],
      disposed: true,
      afterCutoffLoss: { items: 0, bytes: 0 }
    },
    declaration
  )
  expect(claim.streamEnds[0]).toMatchObject({
    error: {
      code: 'platform.failure',
      platform: { code: 'cccd-failed', safeMessage: 'Radio failure', metadata: { status: 133 } }
    }
  })
})
