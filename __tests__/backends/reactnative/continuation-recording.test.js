const {
  parseContinuationRecordingBatch,
  parseContinuationRecordingRecords,
  createContinuationRecordingController,
  createNativeContinuationRecordingController
} = require('../../../src/core/continuation-recording')

const selector = {
  serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
  serviceOccurrence: 1,
  characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
  characteristicOccurrence: 1
}
const metadata = () => ({
  session: {
    peerId: 'peer',
    sessionId: '1',
    backendInstanceId: 'backend',
    sessionStartedAtUnixNs: '123',
    sessionEpoch: 'backend:1:123'
  },
  consumer: {
    consumer: 'ubm-continuation-0',
    peerId: 'peer',
    connectionGeneration: '1',
    databaseGeneration: '2',
    selector: { ...selector }
  }
})
const entry = ordinal => ({
  ordinal,
  metadata: metadata(),
  record: { t: 'value', consumer: 'ubm-continuation-0', valueB64: 'AEg=', delivery: 'notification' }
})
const size = records => records.reduce((total, record) => total + Buffer.byteLength(JSON.stringify(record)), 0)
const batch = () => {
  const records = [entry(4), entry(5)]
  return { token: 'h10:1', bytes: size(records), more: false, records }
}

test('canonical offline row-array decoding needs no fabricated prepare token and preserves raw byte accounting', () => {
  const input = batch()
  const rows = parseContinuationRecordingRecords(input.records, { maxItems: 20, maxBytes: 2000 })
  expect(rows.bytes).toBe(input.bytes)
  expect(rows.records).toEqual(parseContinuationRecordingBatch(input, { maxItems: 20, maxBytes: 2000 }).records)
  expect(rows).not.toHaveProperty('token')
  expect(input.records[0].record.valueB64).toBe('AEg=')
  expect(() => parseContinuationRecordingRecords(input.records, { maxItems: 1, maxBytes: 2000 })).toThrow()
  expect(() => parseContinuationRecordingRecords(input.records, { maxItems: 20, maxBytes: input.bytes - 1 })).toThrow()
  input.records[0].record.valueB64 = '?'
  expect(() => parseContinuationRecordingRecords(input.records, { maxItems: 20, maxBytes: 2000 })).toThrow()
})

test('durable batches expose owned bytes and historical generation metadata without acknowledging', () => {
  const input = batch()
  const result = parseContinuationRecordingBatch(input, { maxItems: 20, maxBytes: 2000 })
  expect(result.records[0].record.value).toEqual(new Uint8Array([0, 72]))
  expect(result.records[0].record.valueB64).toBeUndefined()
  input.records[0].metadata.consumer.selector.serviceOccurrence = 9
  expect(result.records[0].metadata.consumer.selector.serviceOccurrence).toBe(1)
  expect(result.records[0].metadata.session.sessionEpoch).toBe('backend:1:123')
  expect(result.token).toBe('h10:1')
})

test('registration and control records retain one shared cursor and explicit nullable consumer metadata', () => {
  const value = batch()
  value.records[0].record = { t: 'consumer-registration', consumer: 'ubm-continuation-0' }
  value.records[1].metadata.consumer = null
  value.records[1].record = { t: 'ingress-drop', class: 'control', count: 1 }
  value.bytes = size(value.records)
  expect(parseContinuationRecordingBatch(value, { maxItems: 20, maxBytes: 2000 }).records).toHaveLength(2)
  expect(
    parseContinuationRecordingBatch(
      { token: null, records: [], bytes: 0, more: false },
      { maxItems: 20, maxBytes: 2000 }
    ).token
  ).toBeNull()
})

test('rejects falsely reported serialized sizes and accounts UTF-8 metadata before returning data', () => {
  const value = batch()
  expect(() => parseContinuationRecordingBatch({ ...value, bytes: 0 }, { maxItems: 20, maxBytes: 2000 })).toThrow()
  value.records[0].metadata.session.sessionEpoch = 'é'.repeat(100)
  value.bytes = size(value.records)
  expect(parseContinuationRecordingBatch(value, { maxItems: 20, maxBytes: 2000 }).bytes).toBe(value.bytes)
  expect(() =>
    parseContinuationRecordingBatch({ ...value, bytes: 1 }, { maxItems: 20, maxBytes: value.bytes - 1 })
  ).toThrow()
})

test.each(['gap', 'consumer', 'peer', 'base64', 'null-metadata', 'unknown', 'token', 'bounds'])(
  'refuses malformed durable batch (%s)',
  mutation => {
    const value = batch()
    if (mutation === 'gap') value.records[1].ordinal = 7
    if (mutation === 'consumer') value.records[0].record.consumer = 'another'
    if (mutation === 'peer') value.records[0].metadata.consumer.peerId = 'another'
    if (mutation === 'base64') value.records[0].record.valueB64 = '?'
    if (mutation === 'null-metadata') value.records[0].metadata.consumer = null
    if (mutation === 'unknown') value.records[0].record.ignored = true
    if (mutation === 'token') value.token = null
    if (mutation === 'bounds') value.bytes = 2001
    expect(() => parseContinuationRecordingBatch(value, { maxItems: 20, maxBytes: 2000 })).toThrow()
  }
)

test('recording prepare never acknowledges, releases, or silently deletes; bounds validate before dispatch', async () => {
  const access = {
    prepare: jest.fn(async () => batch()),
    acknowledge: jest.fn(async (id, token) => ({ token, acknowledged: true, records: 2 })),
    clear: jest.fn(async () => ({ cleared: true, records: 2 }))
  }
  const controller = createContinuationRecordingController(access)
  const prepared = await controller.prepare('h10', { maxItems: 20, maxBytes: 2000 })
  expect(access.acknowledge).not.toHaveBeenCalled()
  expect(access.clear).not.toHaveBeenCalled()
  expect(await controller.acknowledge('h10', prepared.token)).toEqual({
    token: 'h10:1',
    acknowledged: true,
    records: 2
  })
  for (const options of [
    { maxItems: 2049, maxBytes: 2000 },
    { maxItems: 2, maxBytes: 4194305 },
    { maxItems: 0, maxBytes: 1 }
  ]) {
    await expect(controller.prepare('h10', options)).rejects.toMatchObject({ code: 'argument.invalid' })
  }
  await expect(controller.prepare('../escape', { maxItems: 20, maxBytes: 2000 })).rejects.toMatchObject({
    code: 'argument.invalid'
  })
  expect(access.prepare).toHaveBeenCalledTimes(1)
  access.acknowledge.mockResolvedValue({ token: 'another', acknowledged: true, records: 2 })
  await expect(controller.acknowledge('h10', 'h10:1')).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('status distinguishes persisted collection failure from retryable local diagnostic history', async () => {
  const diagnostic = {
    kind: 'storage.io',
    detail: 'writer busy',
    operation: 'append',
    sqliteExtendedCode: 5,
    sqliteCode: 'DatabaseBusy'
  }
  const snapshot = {
    recordingId: 'h10',
    phase: 'recording',
    accepting: true,
    records: 2,
    bytes: 700,
    lostRecords: 0,
    maxBytes: 1048576,
    maxRecords: 100,
    encrypted: false,
    runtimeFailure: { ...diagnostic, persisted: false },
    collectionFailure: null
  }
  const access = {
    status: jest.fn(async () => snapshot),
    stop: jest.fn(async () => ({ ...snapshot, phase: 'stopped', accepting: false, radioRelease: 'not-requested' }))
  }
  const controller = createContinuationRecordingController(access)
  expect((await controller.status('h10')).accepting).toBe(true)
  expect((await controller.stop('h10')).phase).toBe('stopped')
  access.status.mockResolvedValue({
    ...snapshot,
    accepting: false,
    collectionFailure: { ...diagnostic, persisted: false, persistenceFailure: diagnostic }
  })
  expect((await controller.status('h10')).collectionFailure.persistenceFailure.sqliteExtendedCode).toBe(5)
  access.status.mockResolvedValue({ ...snapshot, collectionFailure: { ...diagnostic, persisted: true } })
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  access.status.mockResolvedValue({ ...snapshot, recordingId: 'other' })
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  access.stop.mockResolvedValue(snapshot)
  await expect(controller.stop('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('native recording control preserves receiver and decodes envelopes without opening a radio', async () => {
  class Store {
    id = 'h10'
    async prepare(id) {
      expect(id).toBe(this.id)
      return JSON.stringify({ ok: true, value: batch() })
    }
  }
  const store = new Store()
  const controller = createNativeContinuationRecordingController(store, 'ubm-desktop')
  expect((await controller.prepare('h10', { maxItems: 20, maxBytes: 2000 })).records).toHaveLength(2)
  store.prepare = async () =>
    JSON.stringify({
      ok: false,
      error: { code: 'platform.failure', domain: 'platform', operation: 'continuation.recording', detail: 'disk full' },
      commit: null,
      retryability: 'never'
    })
  await expect(controller.prepare('h10', { maxItems: 20, maxBytes: 2000 })).rejects.toMatchObject({
    code: 'platform.failure',
    operation: 'continuation.recording'
  })
})

test('native recording prepare accepts a valid retained prefix larger than the ordinary wire envelope and never auto-acks', async () => {
  const records = Array.from({ length: 1600 }, (_, index) => {
    const record = entry(index + 1)
    record.record.valueB64 = Buffer.alloc(240, 72).toString('base64')
    return record
  })
  const held = { token: 'h10:oversized-prefix', records, bytes: size(records), more: true }
  const text = JSON.stringify({ ok: true, value: held })
  expect(Buffer.byteLength(text)).toBeGreaterThan(1048576)
  const { parseInvokeEnvelope } = require('../../../src/backends/reactnative/rust-core-wire')
  expect(parseInvokeEnvelope(text, 'counters.describe').error.normalized.code).toBe('bytes.too-large')
  const access = { prepare: jest.fn(async () => text), acknowledge: jest.fn() }
  const controller = createNativeContinuationRecordingController(access, 'android')
  const first = await controller.prepare('h10', { maxItems: 2048, maxBytes: 4194304 })
  const replay = await controller.prepare('h10', { maxItems: 2048, maxBytes: 4194304 })
  expect(first.token).toBe(held.token)
  expect(replay).toEqual(first)
  expect(first.records).toHaveLength(1600)
  expect(access.acknowledge).not.toHaveBeenCalled()
})

test.each(['status', 'prepare'])('native recording %s retains its own bounded operation error', async operation => {
  const controller = createNativeContinuationRecordingController(
    { [operation]: async () => ' '.repeat(5 * 1024 * 1024) },
    'android'
  )
  const pending =
    operation === 'prepare'
      ? controller.prepare('h10', { maxItems: 2048, maxBytes: 4194304 })
      : controller.status('h10')
  await expect(pending).rejects.toMatchObject({
    code: 'bytes.too-large',
    operation: `react-native-rust-core.wire.continuation.recording.${operation}.envelope`
  })
})

test('larger prepare allowance does not permit malformed envelope fields or non-null write commitment', async () => {
  const access = { prepare: jest.fn(async () => JSON.stringify({ ok: true, value: batch(), ignored: true })) }
  const controller = createNativeContinuationRecordingController(access, 'android')
  await expect(controller.prepare('h10', { maxItems: 20, maxBytes: 2000 })).rejects.toMatchObject({
    code: 'protocol.malformed'
  })
  access.prepare.mockResolvedValue(
    JSON.stringify({
      ok: false,
      error: {
        code: 'platform.failure',
        domain: 'platform',
        operation: 'continuation.recording.prepare',
        detail: null
      },
      commit: 'confirmed',
      retryability: 'never'
    })
  )
  await expect(controller.prepare('h10', { maxItems: 20, maxBytes: 2000 })).rejects.toMatchObject({
    code: 'protocol.malformed'
  })
})
