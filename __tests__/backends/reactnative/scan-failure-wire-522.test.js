const { parseDrainText } = require('../../../src/backends/reactnative/rust-core-wire')

const error = {
  code: 'adapter.powered-off',
  domain: 'adapter',
  operation: 'scan.start',
  retryability: 'never',
  detail: 'adapter powered off',
  platform: { domain: 'corebluetooth', code: 'poweredOff', message: 'adapter powered off', metadata: { state: 'off' } }
}
const legacy = { t: 'scan-end', ordinal: 1, operationId: 'membership-1', reason: 'source-failed' }
const modern = { ...legacy, startOperationId: 'start-1', version: 2, source: 'adapter', generation: '42', error }
const batch = record => JSON.stringify({ more: false, controlLost: 0, records: [record] })
const decode = (record, version) => parseDrainText(batch(record), null, version)

test('legacy scan end is accepted only without unexplained fields', () => {
  expect(decode(legacy, 1).ok).toBe(true)
  expect(decode({ ...legacy, error }, 1).ok).toBe(false)
  expect(decode({ ...legacy, version: 1 }, 1).ok).toBe(false)
})

test('v2 is accepted only after negotiation and retains the exact failure evidence', () => {
  expect(decode(modern, 1).ok).toBe(false)
  expect(decode(modern, 2)).toMatchObject({ ok: true, value: { records: [modern] } })
})

test.each([
  { ...modern, version: 3 },
  { ...modern, extra: 'unknown' },
  { ...modern, error: { ...error, extra: 'unknown' } },
  { ...modern, generation: 'not-a-generation' },
  { ...modern, source: 'unknown' },
  { ...modern, reason: 'operation-aborted' },
  { ...modern, startOperationId: '' }
])('rejects incompatible or unexplained failure evidence %#', record => {
  expect(decode(record, 2).ok).toBe(false)
})

test('a v2 session cannot silently downgrade a source failure to v1', () => {
  expect(decode(legacy, 2).ok).toBe(false)
})

test('negotiation rejects unknown versions and fields without echoing their content', () => {
  const { parseScanEndVersion } = require('../../../src/backends/reactnative/rust-core-wire')
  expect(parseScanEndVersion('{"version":1}')).toMatchObject({ ok: true, value: 1 })
  expect(parseScanEndVersion('{"version":2}')).toMatchObject({ ok: true, value: 2 })
  expect(parseScanEndVersion('{"version":3}').error.normalized.code).toBe('protocol.incompatible')
  const rejected = parseScanEndVersion('{"version":2,"token":"private-value"}')
  expect(rejected.error.normalized.code).toBe('protocol.malformed')
  expect(JSON.stringify(rejected.error.normalized)).not.toContain('private-value')
})

test('v2 checks ordinal ordering across batches', () => {
  const result = parseDrainText(batch(modern), 1, 2)
  expect(result.ok).toBe(false)
})

test('native metadata cannot impersonate the reserved scan provenance entries', () => {
  const forged = {
    ...modern,
    error: { ...error, platform: { ...error.platform, metadata: { 'ubm.scan.source': 'forged' } } }
  }
  expect(decode(forged, 2).ok).toBe(false)
})
