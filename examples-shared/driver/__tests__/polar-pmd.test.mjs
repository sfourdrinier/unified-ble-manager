import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  EcgStreamStats,
  PmdParseError,
  PmdControlPointResponseAssembler,
  buildGetEcgSettingsCommand,
  buildGetAccSettingsCommand,
  buildStartAccCommand,
  buildStopAccCommand,
  buildStartEcgCommand,
  buildStopEcgCommand,
  parseControlPointMessage,
  parseAccFrame,
  parseEcgFrame,
  parsePmdFeatures,
  parsePmdSettings
} from '../polar-pmd.ts'

test('multipart responses assemble correlated copied parameters only at the final fragment', () => {
  const response = new PmdControlPointResponseAssembler(1, 2, 7)
  const first = parseControlPointMessage(new Uint8Array([0xf0, 1, 2, 0, 1, 0, 1, 200]))
  assert.equal(response.push(first, 7), null)
  first.parameters.fill(99)
  assert.equal(response.push(parseControlPointMessage(new Uint8Array([0xf0, 1, 0, 0, 0, 9])), 7), null)
  assert.equal(response.push(parseControlPointMessage(new Uint8Array([0xf0, 1, 2, 0, 0, 9])), 6), null)
  const final = response.push(parseControlPointMessage(new Uint8Array([0xf0, 1, 2, 0, 0, 0, 1, 1, 16, 0])), 7)
  assert.equal(final.more, false)
  assert.deepEqual(parsePmdSettings(final.parameters), { SAMPLE_RATE: [200], RESOLUTION: [16] })
  assert.throws(() => response.push(first, 7), PmdParseError)
})

test('multipart rejection discards successful prefix and response assembly is bounded', () => {
  const packet = more => parseControlPointMessage(new Uint8Array([0xf0, 1, 0, 0, more, 1]))
  const rejected = new PmdControlPointResponseAssembler(1, 0, 1)
  rejected.push(packet(1), 1)
  const failure = rejected.push(parseControlPointMessage(new Uint8Array([0xf0, 1, 0, 6])), 1)
  assert.equal(failure.status, 6)
  assert.equal(failure.parameters.length, 0)
  const flood = new PmdControlPointResponseAssembler(1, 0, 1)
  for (let index = 0; index < 64; index++) assert.equal(flood.push(packet(1), 1), null)
  assert.throws(() => flood.push(packet(0), 1), /fragment limit/)
  const bytes = new PmdControlPointResponseAssembler(1, 0, 1)
  assert.throws(() => bytes.push({ ...packet(0), parameters: new Uint8Array(32769) }, 1), /byte limit/)
})

test('H10 ACC commands encode all twelve supported rate/range settings with 16-bit resolution', () => {
  for (const sampleRateHz of [25, 50, 100, 200]) {
    for (const rangeG of [2, 4, 8]) {
      assert.deepEqual(
        [...buildStartAccCommand({ sampleRateHz, resolutionBits: 16, rangeG })],
        [2, 2, 0, 1, sampleRateHz, 0, 1, 1, 16, 0, 2, 1, rangeG, 0]
      )
    }
  }
  assert.deepEqual([...buildGetAccSettingsCommand()], [1, 2])
  assert.deepEqual([...buildStopAccCommand()], [3, 2])
})

test('H10 ACC builder refuses unsupported, missing or non-finite settings', () => {
  const supported = { sampleRateHz: 25, resolutionBits: 16, rangeG: 2 }
  for (const sampleRateHz of [0, 24, 26, 400, 25.5, NaN, Infinity, '25', undefined]) {
    assert.throws(() => buildStartAccCommand({ ...supported, sampleRateHz }), RangeError)
  }
  for (const rangeG of [0, 1, 3, 16, NaN, undefined])
    assert.throws(() => buildStartAccCommand({ ...supported, rangeG }), RangeError)
  for (const resolutionBits of [8, 14, 24, undefined])
    assert.throws(() => buildStartAccCommand({ ...supported, resolutionBits }), RangeError)
})

test('ACC decoder matches the official SDK raw16 vector', () => {
  const frame = parseAccFrame(
    new Uint8Array([
      0x02, 0, 0x94, 0x35, 0x77, 0, 0, 0, 0, 1, 0xf7, 0xff, 0xff, 0xff, 0xe7, 3, 0xf8, 0xff, 0xfe, 0xff, 0xe5, 3
    ])
  )
  assert.equal(frame.timestampNs, 2000000000n)
  assert.deepEqual(frame.samplesMilliG, [
    { x: -9, y: -1, z: 999 },
    { x: -8, y: -2, z: 997 }
  ])
})

// Literal vectors, independent of the simulator's encoder. Polar SDK AccData
// raw TYPE_0/1/2 are signed 8/16/24-bit x,y,z values in milliG.
for (const [frameType, payload, expected] of [
  [
    0,
    [0x80, 0x7f, 0xff, 0, 1, 0xfe],
    [
      { x: -128, y: 127, z: -1 },
      { x: 0, y: 1, z: -2 }
    ]
  ],
  [
    1,
    [0, 0x80, 0xff, 0x7f, 0xff, 0xff, 0xe8, 3, 0x18, 0xfc, 0, 0],
    [
      { x: -32768, y: 32767, z: -1 },
      { x: 1000, y: -1000, z: 0 }
    ]
  ],
  [2, [0, 0, 0x80, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff], [{ x: -8388608, y: 8388607, z: -1 }]]
]) {
  test(`ACC raw type ${frameType} preserves signed axis extrema, milliG and uint64 timestamp`, () => {
    const packet = new Uint8Array([2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, frameType, ...payload])
    const backing = new Uint8Array(packet.length + 7)
    backing.set(packet, 3)
    const frame = parseAccFrame(backing.subarray(3, 3 + packet.length))
    assert.deepEqual(frame, {
      timestampNs: 18446744073709551615n,
      frameType,
      compressed: false,
      samplesMilliG: expected
    })
    backing.fill(0)
    assert.deepEqual(frame.samplesMilliG, expected, 'decoded data must not alias the input')
  })
}

test('ACC rejects truncated headers, wrong type, compressed/unknown formats and ragged axes', () => {
  for (let size = 0; size < 10; size++) assert.throws(() => parseAccFrame(new Uint8Array(size)), PmdParseError)
  const frame = (type, payload = [], measurement = 2) =>
    new Uint8Array([measurement, 0, 0, 0, 0, 0, 0, 0, 0, type, ...payload])
  assert.throws(() => parseAccFrame(frame(0, [1, 2, 3], 0)), /expected ACC/)
  for (const type of [0x80, 0x81, 0x82]) assert.throws(() => parseAccFrame(frame(type, [1, 2, 3])), /compressed/)
  assert.throws(() => parseAccFrame(frame(3, [1, 2, 3])), /frame type 3/)
  for (const type of [0, 1, 2]) {
    const stride = (type + 1) * 3
    for (let size = 0; size < stride * 2; size++) {
      if (size === stride) continue
      assert.throws(() => parseAccFrame(frame(type, new Array(size).fill(0))), /non-zero multiple/)
    }
  }
})

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
  const ok = parseControlPointMessage(
    new Uint8Array([0xf0, 0x02, 0x00, 0x00, 0x00, 0x05, 0x01, 0x40, 0x9c, 0x00, 0x00])
  )
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
  assert.deepEqual(parsePmdFeatures(new Uint8Array([0x0f, 0x05, 0x00])), {
    ecg: true,
    ppg: false,
    acc: true,
    ppi: false
  })
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
