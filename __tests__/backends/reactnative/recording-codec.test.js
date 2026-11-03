// __tests__/backends/reactnative/recording-codec.test.js
const { serializeContinuationRecordingRecords, deserializeContinuationRecordingRecords } = require('../../../src/index')
const { parseContinuationRecordingRecords } = require('../../../src/core/continuation-recording')

const limits = { maxItems: 10, maxBytes: 65536 }
const session = {
  peerId: 'peer',
  sessionId: '1',
  backendInstanceId: 'backend',
  sessionStartedAtUnixNs: '123',
  sessionEpoch: 'epoch'
}
const consumer = {
  consumer: 'consumer',
  peerId: 'peer',
  connectionGeneration: 'cg',
  databaseGeneration: 'dg',
  selector: {
    serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
    serviceOccurrence: 1,
    characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
    characteristicOccurrence: 1
  }
}

function rows() {
  return [
    { ordinal: 1, metadata: { session, consumer }, record: { t: 'consumer-registration', consumer: 'consumer' } },
    {
      ordinal: 2,
      metadata: { session, consumer },
      record: {
        t: 'value',
        consumer: 'consumer',
        valueB64: 'AP9/gA==',
        delivery: 'notification',
        receipt: { receivedAtUtcMs: -0.5, receivedAtMonotonicMs: 42.25, clockId: 'clock', origin: 'native' }
      }
    },
    {
      ordinal: 3,
      metadata: { session, consumer: null },
      record: { t: 'ingress-drop', class: 'notification', count: 2 }
    },
    {
      ordinal: 4,
      metadata: { session, consumer: null },
      record: {
        t: 'adv',
        operationId: 'scan',
        startOperationId: 'start',
        peerId: 'peer',
        localName: null,
        rssi: -40,
        txPower: null,
        serviceUuids: null,
        manufacturerData: [{ companyId: 123, payloadB64: 'AP8=' }],
        serviceData: [{ uuid: '0000180d-0000-1000-8000-00805f9b34fb', payloadB64: 'AQI=' }],
        connectable: null,
        solicitedServiceUuids: null,
        overflowServiceUuids: null,
        appearance: null,
        rawRecordB64: 'gP8=',
        observedAtMs: 3,
        sourceTimestampMs: null
      }
    }
  ]
}

test('public offline codec round-trips native bytes, receipts, metadata and controls without cursor authority', () => {
  const native = parseContinuationRecordingRecords(rows(), limits)
  const serialized = serializeContinuationRecordingRecords(native.records)
  const decoded = deserializeContinuationRecordingRecords(serialized, limits)
  expect(decoded.records).toEqual(native.records)
  expect(decoded.records[1].record.value).toEqual(new Uint8Array([0, 255, 127, 128]))
  expect(decoded.records[3].record.manufacturerData[0].payload).toEqual(new Uint8Array([0, 255]))
  expect(decoded.records[3].record.serviceData[0].payload).toEqual(new Uint8Array([1, 2]))
  expect(decoded.records[3].record.rawRecord).toEqual(new Uint8Array([128, 255]))
  expect(decoded.bytes).toBe(
    JSON.parse(serialized).reduce((sum, row) => sum + Buffer.byteLength(JSON.stringify(row)), 0)
  )
  expect(decoded).not.toHaveProperty('token')
})

test('offline codec rejects malformed rows and out-of-bound input', () => {
  const serialized = serializeContinuationRecordingRecords(parseContinuationRecordingRecords(rows(), limits).records)
  expect(() => deserializeContinuationRecordingRecords(serialized, { maxItems: 2, maxBytes: 65536 })).toThrow()
  expect(() => deserializeContinuationRecordingRecords(serialized, { maxItems: 10, maxBytes: 20 })).toThrow()
  expect(() => deserializeContinuationRecordingRecords('[', limits)).toThrow()
  const invalid = rows()
  invalid[1].record.valueB64 = '?!'
  expect(() => deserializeContinuationRecordingRecords(JSON.stringify(invalid), limits)).toThrow()
  invalid[1].record.valueB64 = 'AA=='
  invalid[1].metadata.consumer = { ...invalid[1].metadata.consumer, consumer: 'wrong' }
  expect(() => deserializeContinuationRecordingRecords(JSON.stringify(invalid), limits)).toThrow()
  const decoded = parseContinuationRecordingRecords(rows(), limits).records
  const changedOrdinal = [{ ...decoded[0], ordinal: 9 }]
  expect(() => serializeContinuationRecordingRecords(changedOrdinal)).toThrow()
  const changedPayload = [{ ...decoded[0], record: { ...decoded[0].record, unexpected: true } }]
  expect(() => serializeContinuationRecordingRecords(changedPayload)).toThrow()
})
