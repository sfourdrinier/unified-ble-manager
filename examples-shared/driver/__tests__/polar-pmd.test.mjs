import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  EcgStreamStats,
  PmdParseError,
  buildGetEcgSettingsCommand,
  buildStartEcgCommand,
  buildStopEcgCommand,
  parseControlPointMessage,
  parseEcgFrame,
  parsePmdFeatures,
  parsePmdSettings
} from '../polar-pmd.ts'

// Reference: polarofficial/polar-ble-sdk (Android) BlePMDClient.startMeasurement,
// PmdSetting.serializeSelected, PmdRecordingType.asBitField, PmdControlPointCommand:
// [REQUEST_MEASUREMENT_START=0x02][ONLINE(0<<7)|ECG(0x00)]
// [SAMPLE_RATE=0x00][count=1][130 as uint16 LE] [RESOLUTION=0x01][count=1][14 as uint16 LE]
test('start ECG command is the Polar SDK REQUEST_MEASUREMENT_START for 130 Hz / 14 bit', () => {
  assert.deepEqual([...buildStartEcgCommand()], [0x02, 0x00, 0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0e, 0x00])
})

test('stop and get-settings commands follow the Polar opcodes', () => {
  assert.deepEqual([...buildStopEcgCommand()], [0x03, 0x00])
  assert.deepEqual([...buildGetEcgSettingsCommand()], [0x01, 0x00])
})

test('control point response parses status, more flag and parameters', () => {
  const ok = parseControlPointMessage(new Uint8Array([0xf0, 0x02, 0x00, 0x00, 0x00, 0x05, 0x01, 0x40, 0x9c, 0x00, 0x00]))
  assert.deepEqual(ok, {
    kind: 'response',
    opCode: 0x02,
    measurementType: 0x00,
    status: 0,
    statusName: 'SUCCESS',
    more: false,
    parameters: new Uint8Array([0x05, 0x01, 0x40, 0x9c, 0x00, 0x00])
  })
  const failed = parseControlPointMessage(new Uint8Array([0xf0, 0x02, 0x00, 0x06]))
  assert.equal(failed.statusName, 'ERROR_ALREADY_IN_STATE')
  assert.deepEqual([...failed.parameters], [])
  assert.equal(parseControlPointMessage(new Uint8Array([0xf0, 0x02, 0x00, 0x63])).statusName, 'UNKNOWN_ERROR')
})

test('device-initiated stop is reported as online-measurement-stopped with its types', () => {
  assert.deepEqual(parseControlPointMessage(new Uint8Array([0x01, 0x00])), {
    kind: 'online-measurement-stopped',
    measurementTypes: [0]
  })
})

test('short or unknown control point messages throw PmdParseError', () => {
  assert.throws(() => parseControlPointMessage(new Uint8Array([0xf0, 0x02])), PmdParseError)
  assert.throws(() => parseControlPointMessage(new Uint8Array([])), PmdParseError)
  assert.throws(() => parseControlPointMessage(new Uint8Array([0x7a, 0x00])), PmdParseError)
})

test('settings TLV parses sample rates and resolutions with Polar field sizes', () => {
  assert.deepEqual(parsePmdSettings(new Uint8Array([0x00, 0x01, 0x82, 0x00, 0x01, 0x01, 0x0e, 0x00])), {
    SAMPLE_RATE: [130],
    RESOLUTION: [14]
  })
  assert.deepEqual(parsePmdSettings(new Uint8Array([0x05, 0x01, 0x00, 0x00, 0x80, 0x3f])), { FACTOR: [0x3f800000] })
  assert.throws(() => parsePmdSettings(new Uint8Array([0x00, 0x02, 0x82])), PmdParseError)
  assert.throws(() => parsePmdSettings(new Uint8Array([0x7f, 0x01, 0x00])), PmdParseError)
})

test('feature read reports ECG support from byte 1 bit 0', () => {
  assert.deepEqual(parsePmdFeatures(new Uint8Array([0x0f, 0x05, 0x00])), { ecg: true, ppg: false, acc: true, ppi: false })
  assert.throws(() => parsePmdFeatures(new Uint8Array([0x0f])), PmdParseError)
})

function ecgFrame(timestampNs, samples, frameType = 0x00, measurementType = 0x00) {
  const bytes = new Uint8Array(10 + samples.length * 3)
  bytes[0] = measurementType
  new DataView(bytes.buffer).setBigUint64(1, timestampNs, true)
  bytes[9] = frameType
  samples.forEach((sample, index) => {
    const raw = sample < 0 ? sample + 0x1000000 : sample
    bytes[10 + index * 3] = raw & 0xff
    bytes[11 + index * 3] = (raw >> 8) & 0xff
    bytes[12 + index * 3] = (raw >> 16) & 0xff
  })
  return bytes
}

test('ECG frame type 0 decodes signed 24-bit little-endian microvolt samples and the uint64 timestamp', () => {
  const frame = parseEcgFrame(ecgFrame(599_000_000_000_000_000n, [0, 1, -1, 8_388_607, -8_388_608, -150]))
  assert.equal(frame.timestampNs, 599_000_000_000_000_000n)
  assert.equal(frame.frameType, 0)
  assert.equal(frame.compressed, false)
  assert.deepEqual(frame.samplesMicroVolts, [0, 1, -1, 8_388_607, -8_388_608, -150])
})

test('ECG frame rejects non-ECG, compressed, unsupported types and ragged payloads', () => {
  assert.throws(() => parseEcgFrame(new Uint8Array(9)), PmdParseError)
  assert.throws(() => parseEcgFrame(ecgFrame(1n, [1], 0x00, 0x02)), /measurement type 2/)
  assert.throws(() => parseEcgFrame(ecgFrame(1n, [1], 0x80)), /compressed/)
  assert.throws(() => parseEcgFrame(ecgFrame(1n, [1], 0x01)), /frame type 1/)
  assert.throws(() => parseEcgFrame(ecgFrame(1n, [])), /multiple of 3/)
  const ragged = ecgFrame(1n, [1, 2]).slice(0, 14)
  assert.throws(() => parseEcgFrame(ragged), /multiple of 3/)
})

test('stream stats count samples, receipt rate, timestamp gaps and notification sequence drops', () => {
  const stats = new EcgStreamStats(130)
  const perFrame = 73
  const frameNs = BigInt(Math.round((perFrame * 1e9) / 130))
  const t0 = 1_000_000_000n
  const frame = ts => parseEcgFrame(ecgFrame(ts, new Array(perFrame).fill(5)))
  assert.deepEqual(stats.record(frame(t0), 0, 1), { timestampGap: null, sequenceGap: null })
  assert.deepEqual(stats.record(frame(t0 + frameNs), 900, 2), { timestampGap: null, sequenceGap: null })
  const gapped = stats.record(frame(t0 + frameNs * 4n), 1_700, 5)
  assert.equal(gapped.sequenceGap.missingNotifications, 2)
  assert.equal(gapped.timestampGap.estimatedMissingSamples, perFrame * 2)
  const summary = stats.summary(1_700)
  assert.equal(summary.frames, 3)
  assert.equal(summary.samples, perFrame * 3)
  assert.equal(summary.timestampGaps, 1)
  assert.equal(summary.estimatedMissingSamples, perFrame * 2)
  assert.equal(summary.missingNotifications, 2)
  assert.equal(summary.samplesLastSecond, perFrame * 2)
  // samples after the first frame over the sensor time they span: 146 samples in 4 frame periods
  assert.ok(Math.abs(summary.sensorSampleRateHz - 65) < 0.5)
})
