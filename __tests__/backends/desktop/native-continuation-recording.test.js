const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')

test('store opening transport rejection is a public typed platform failure', async () => {
  await expect(openNativeContinuationRecordings({ openRecordingStore: async () => { throw new Error('worker rejected') } }, '/private/app'))
    .rejects.toMatchObject({ code: 'platform.failure', operation: 'continuation.recording.open', platform: { domain: 'ubm-desktop' } })
})

test('opens durable recording access without touching radio admission or enumeration', async () => {
  const calls = []
  const binding = {
    async openRecordingStore(directory) {
      calls.push(directory)
      return {
        async prepare(id) {
          return JSON.stringify({ ok: true, value: { token: null, records: [], bytes: 0, more: false } })
        }
      }
    },
    openProduction: jest.fn(() => {
      throw new Error('radio unavailable')
    }),
    listAdapters: jest.fn(() => {
      throw new Error('Bluetooth denied')
    })
  }
  const recordings = await openNativeContinuationRecordings(binding, '/private/app/recordings')
  expect(await recordings.prepare('recording-1', { maxItems: 10, maxBytes: 4096 })).toEqual({
    token: null,
    records: [],
    bytes: 0,
    more: false
  })
  expect(calls).toEqual(['/private/app/recordings'])
  expect(binding.openProduction).not.toHaveBeenCalled()
  expect(binding.listAdapters).not.toHaveBeenCalled()
})

test('missing recording support reports unsupported rather than an empty recording', async () => {
  await expect(openNativeContinuationRecordings({}, '/private/app/recordings')).rejects.toMatchObject({
    code: 'capability.unsupported'
  })
})

test('diagnostic recovery preserves exact bytes and requires the complete lifecycle transport', async () => {
  const occurrence = 'ab'.repeat(40)
  const payload = '{"loss":{"items":2}}'
  const store = {
    diagnosticSummaryPage: jest.fn(async () => JSON.stringify({ ok: true, value: { bytes: [255], totalBytes: '1' } })),
    diagnosticOccurrencesPage: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrences: [occurrence] } })),
    diagnosticOccurrencePage: jest.fn(async () => JSON.stringify({ ok: true, value: { bytes: Array.from(Buffer.from(payload)), totalBytes: String(Buffer.byteLength(payload)), durable: true, reclaimed: false } })),
    retryDiagnosticOccurrence: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, retried: true } })),
    acknowledgeDiagnosticOccurrence: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, transition: 'acknowledge' } })),
    retireDiagnosticOccurrence: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, transition: 'retire' } })),
    confirmDiagnosticRetirement: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, transition: 'confirm' } })),
    releaseDiagnosticConfirmation: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, transition: 'release' } })),
    transitionDiagnosticOccurrenceChunk: jest.fn(async () => JSON.stringify({ ok: true, value: { occurrence, transition: 'acknowledge', offset: '1', totalBytes: '1', complete: true } }))
  }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  expect(await recordings.diagnostics.summaryPage('recording', '0', 1)).toEqual({ bytes: [255], totalBytes: '1' })
  expect(await recordings.diagnostics.occurrencesPage('recording', null, 8)).toEqual([occurrence])
  expect(await recordings.diagnostics.occurrencePage('recording', occurrence, '0', 4096)).toEqual({ bytes: Array.from(Buffer.from(payload)), totalBytes: String(Buffer.byteLength(payload)), durable: true, reclaimed: false })
  await recordings.diagnostics.retry('recording', occurrence)
  await recordings.diagnostics.acknowledge('recording', occurrence, payload)
  await recordings.diagnostics.retire('recording', occurrence, payload)
  await recordings.diagnostics.confirmRetirement('recording', occurrence, payload)
  await recordings.diagnostics.releaseConfirmation('recording', occurrence, payload)
  expect(store.releaseDiagnosticConfirmation).toHaveBeenCalledWith('recording', occurrence, payload)
})

test('legacy recovery passes the exact versioned page through ACK and retirement', async () => {
  const page = { version: 1, token: 'recording:1', cursorLast: '1', rawRecords: [{ ordinal: '1', body: [123, 125] }], bytes: 2, more: false }
  const ack = { version: 1, token: page.token, acknowledged: true, records: 1 }
  const retired = { version: 1, token: page.token, retired: true }
  const store = {
    legacyRecordingPrepareExact: jest.fn(async () => JSON.stringify({ ok: true, value: page })),
    legacyRecordingAcknowledgeExact: jest.fn(async () => JSON.stringify({ ok: true, value: ack })),
    legacyRecordingRetireExact: jest.fn(async () => JSON.stringify({ ok: true, value: retired })),
    legacyRecordingConfirmExactRetirement: jest.fn(async () => JSON.stringify({ ok: true, value: retired }))
  }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  const result = await recordings.prepareLegacy('recording', { maxItems: 2, maxBytes: 4096 })
  expect(result.rawRecords).toEqual(page.rawRecords)
  expect(await recordings.acknowledgeLegacy('recording', result)).toEqual(ack)
  expect(await recordings.retireLegacyReceipt('recording', result)).toEqual(retired)
  expect(JSON.parse(store.legacyRecordingAcknowledgeExact.mock.calls[0][1])).toEqual(result)
  expect(JSON.parse(store.legacyRecordingRetireExact.mock.calls[0][1])).toEqual(result)
})

test('legacy recovery restores a crash-retained exact page before any destructive retry', async () => {
  const page = { version: 1, token: 'recording:1', cursorLast: '1', rawRecords: [{ ordinal: '1', body: [123, 125] }], bytes: 2, more: false }
  const recover = jest.fn(async () => JSON.stringify({ ok: true, value: page }))
  const store = {
    legacyRecordingPrepareExact: jest.fn(),
    legacyRecordingRecoverExact: recover,
    legacyRecordingAcknowledgeExact: jest.fn(),
    legacyRecordingRetireExact: jest.fn(),
    legacyRecordingConfirmExactRetirement: jest.fn()
  }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  await expect(recordings.recoverLegacy('recording')).resolves.toEqual(page)
  expect(recover).toHaveBeenCalledWith('recording')
})

test('legacy recovery refuses a store that has only token-only controls', async () => {
  const store = { legacyRecordingPrepare: jest.fn(), legacyRecordingAcknowledge: jest.fn() }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  await expect(recordings.prepareLegacy('recording', { maxItems: 1, maxBytes: 4096 })).rejects.toMatchObject({ code: 'capability.unsupported' })
  expect(store.legacyRecordingPrepare).not.toHaveBeenCalled()
})

test('legacy recovery rejects malformed byte evidence before ACK', async () => {
  const ack = jest.fn()
  const store = {
    legacyRecordingPrepareExact: jest.fn(),
    legacyRecordingAcknowledgeExact: ack,
    legacyRecordingRetireExact: jest.fn(),
    legacyRecordingConfirmExactRetirement: jest.fn()
  }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  const page = { version: 1, token: 'one', cursorLast: '1', rawRecords: [{ ordinal: '1', body: [256] }], bytes: 1, more: false }
  await expect(recordings.acknowledgeLegacy('recording', page)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(ack).not.toHaveBeenCalled()
})

test('raw recovery preserves invalid UTF-8 and exposes length and JSON faults', async () => {
  const chunk = { source: 'records', schemaVersion: 2, ordinal: '1', nextOrdinal: null, offset: '0', length: '2', storedLength: '3', bytes: [255, 123], complete: true, lengthValid: false, lengthFault: { kind: 'storage.corrupt', detail: 'length mismatch', operation: null, sqliteExtendedCode: null, sqliteCode: null }, jsonValidated: true, jsonValid: false, countersValid: null, counterFault: null }
  const inspect = jest.fn(async () => JSON.stringify({ ok: true, value: chunk }))
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => ({ inspectRawRecord: inspect }) }, '/private/app')
  expect(await recordings.inspectRawRecord('recording', 'records', null, '0', 2)).toEqual(chunk)
  expect(inspect).toHaveBeenCalledWith('recording', 'records', null, '0', 2)
})

test('raw recovery rejects contradictory native diagnostics', async () => {
  const chunk = { source: 'records', schemaVersion: 2, ordinal: '1', nextOrdinal: null, offset: '0', length: '2', storedLength: '3', bytes: [255, 123], complete: true, lengthValid: true, lengthFault: null, jsonValidated: true, jsonValid: false, countersValid: null, counterFault: null }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => ({ inspectRawRecord: async () => JSON.stringify({ ok: true, value: chunk }) }) }, '/private/app')
  await expect(recordings.inspectRawRecord('recording', 'records', '1', '0', 2)).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('confirmation forwards the exact retired page before permitting another page', async () => {
  const page = { version: 1, token: 'one', cursorLast: '1', rawRecords: [{ ordinal: '1', body: [123, 125] }], bytes: 2, more: false }
  const confirm = jest.fn(async () => JSON.stringify({ ok: true, value: { version: 1, token: 'one', retired: true } }))
  const store = { legacyRecordingPrepareExact: jest.fn(), legacyRecordingAcknowledgeExact: jest.fn(), legacyRecordingRetireExact: jest.fn(), legacyRecordingConfirmExactRetirement: confirm }
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  expect(await recordings.confirmLegacyRetirement('recording', page)).toEqual({ version: 1, token: 'one', retired: true })
  expect(JSON.parse(confirm.mock.calls[0][1])).toEqual(page)
})

test.each([
  'legacyRecordingPrepareExact',
  'legacyRecordingAcknowledgeExact',
  'legacyRecordingRetireExact',
  'legacyRecordingConfirmExactRetirement'
])('legacy recovery refuses every exact operation when %s is missing', async missing => {
  const page = { version: 1, token: 'one', cursorLast: '1', rawRecords: [{ ordinal: '1', body: [123, 125] }], bytes: 2, more: false }
  const store = {
    legacyRecordingPrepareExact: jest.fn(),
    legacyRecordingAcknowledgeExact: jest.fn(),
    legacyRecordingRetireExact: jest.fn(),
    legacyRecordingConfirmExactRetirement: jest.fn()
  }
  const methods = Object.values(store)
  delete store[missing]
  const recordings = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  for (const operation of [
    () => recordings.prepareLegacy('recording', { maxItems: 1, maxBytes: 4096 }),
    () => recordings.acknowledgeLegacy('recording', page),
    () => recordings.retireLegacyReceipt('recording', page),
    () => recordings.confirmLegacyRetirement('recording', page)
  ]) {
    await expect(operation()).rejects.toMatchObject({ code: 'capability.unsupported' })
  }
  for (const method of methods) expect(method).not.toHaveBeenCalled()
})
