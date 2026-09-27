import assert from 'node:assert/strict'
import test from 'node:test'
import { PmdRecorder, isPmdRecording } from '../pmd-recording.ts'

const packet = (data = {}) => ({
  kind: 'packet',
  peerId: 'SIM H10',
  generation: '1',
  atMs: 10,
  data: { bytesHex: '0200', sensorTimestampNs: '90071992547409930', ...data }
})

test('recording is opt-in, immutable, exportable only after stop, and requires explicit clear', () => {
  const recorder = new PmdRecorder()
  recorder.append(packet())
  assert.equal(recorder.summary().records, 0)
  assert.throws(() => recorder.export(), /stopped/)
  const metadata = { label: 'real', nested: { rangeG: 8 } }
  recorder.start(metadata, 0)
  metadata.nested.rangeG = 2
  const record = packet({ settings: { rangeG: 8 } })
  recorder.append(record)
  record.data.settings.rangeG = 4
  assert.throws(() => recorder.export(), /stopped/)
  assert.throws(() => recorder.clear(), /recording/)
  recorder.stop(20)
  const capture = recorder.export()
  assert.equal(capture.schema, 'ubm-pmd-recording/1')
  assert.equal(capture.metadata.nested.rangeG, 8)
  assert.equal(capture.records[0].data.settings.rangeG, 8)
  assert.equal(capture.records[0].data.sensorTimestampNs, '90071992547409930')
  capture.records[0].data.settings.rangeG = 1
  assert.equal(recorder.export().records[0].data.settings.rangeG, 8)
  assert.equal(isPmdRecording(capture), true)
  assert.throws(() => recorder.start({}, 30), /clear/)
  recorder.clear()
  recorder.start({}, 30)
  assert.equal(recorder.summary().phase, 'recording')
})

test('capacity retains first records and reports every omitted record without unbounded growth', () => {
  const recorder = new PmdRecorder({ maxRecords: 2, maxBytes: 4096 })
  recorder.start({}, 0)
  for (let index = 0; index < 5; index++) recorder.append(packet({ index }))
  assert.equal(recorder.summary().phase, 'capacity-reached')
  assert.equal(recorder.summary().accepting, true)
  assert.equal(recorder.summary().records, 2)
  assert.equal(recorder.summary().omittedRecords, 3)
  assert.deepEqual(recorder.summary().incompleteReasons, ['capacity'])
  assert.deepEqual(
    recorder.export().records.map(record => record.data.index),
    [0, 1]
  )
  recorder.stop(40)
  assert.equal(recorder.summary().accepting, false)
  recorder.append(packet())
  assert.equal(recorder.summary().omittedRecords, 3)
})

test('UTF-8 byte budget includes metadata and whole retained records', () => {
  const recorder = new PmdRecorder({ maxRecords: 100, maxBytes: 180 })
  recorder.start({ label: 'é🫀' }, 0)
  recorder.append(packet({ text: '🫀'.repeat(80) }))
  assert.equal(recorder.summary().records, 0)
  assert.equal(recorder.summary().omittedRecords, 1)
  assert.ok(recorder.summary().bytes <= 180)
  assert.throws(() => new PmdRecorder({ maxRecords: 0, maxBytes: 100 }), /positive/)
  assert.throws(() => new PmdRecorder({ maxRecords: 1, maxBytes: 10 }).start({ label: 'x'.repeat(100) }, 0), /metadata/)
})

test('loss and parse failures remain explicit; invalid JSON and timestamps fail closed', () => {
  const recorder = new PmdRecorder()
  recorder.start({}, 0)
  recorder.append({ ...packet(), kind: 'loss', data: { dropped: 2 } })
  recorder.append({ ...packet(), kind: 'error', data: { message: 'truncated' } })
  assert.throws(() => recorder.append(packet({ value: NaN })), /JSON/)
  assert.throws(() => recorder.append({ ...packet(), atMs: Infinity }), /timestamp/)
  recorder.stop(20)
  assert.deepEqual(recorder.summary().incompleteReasons, ['loss', 'error'])
  assert.equal(isPmdRecording({ schema: 'ubm-pmd-recording/1' }), false)
  const capture = recorder.export()
  assert.equal(isPmdRecording({ ...capture, records: [{ ...capture.records[0], atMs: NaN }] }), false)
})
