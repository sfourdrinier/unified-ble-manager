// example-expo/src/driver/polar-pmd.ts
//
// Polar Measurement Data (PMD) framing for H10 ECG and accelerometer streams. Every byte
// layout here follows Polar's official BLE SDK source (polarofficial/
// polar-ble-sdk, Android `BlePMDClient`, `PmdSetting`, `PmdControlPointCommand`,
// `PmdControlPointResponse`, `PmdDataFrame`, `EcgData`), not guesswork.
// ACC reference: SDK a693e9e944c9bc925addbdd8cf07fb9b28748bf7 AccData.kt,
// documentation/products/PolarH10.md and issue #124 comment 772310984.

export const PMD_SERVICE = 'FB005C80-02E7-F387-1CAD-8ACD2D8DF0C8'
export const PMD_CONTROL_POINT = 'FB005C81-02E7-F387-1CAD-8ACD2D8DF0C8'
export const PMD_DATA = 'FB005C82-02E7-F387-1CAD-8ACD2D8DF0C8'

/** Polar's preferred ATT MTU (ConnectionHandler.POLAR_PREFERRED_MTU). */
export const POLAR_PREFERRED_MTU = 512
export const H10_ECG_SAMPLE_RATE_HZ = 130
export const H10_ECG_RESOLUTION_BITS = 14
export type H10AccSampleRateHz = 25 | 50 | 100 | 200
export type H10AccRangeG = 2 | 4 | 8
export const H10_ACC_SAMPLE_RATES_HZ: readonly H10AccSampleRateHz[] = [25, 50, 100, 200]
export const H10_ACC_RANGES_G: readonly H10AccRangeG[] = [2, 4, 8]
export const H10_ACC_RESOLUTION_BITS = 16

export interface H10AccSettings {
  readonly sampleRateHz: H10AccSampleRateHz
  readonly resolutionBits: typeof H10_ACC_RESOLUTION_BITS
  readonly rangeG: H10AccRangeG
}

const OP_GET_MEASUREMENT_SETTINGS = 0x01
const OP_REQUEST_MEASUREMENT_START = 0x02
const OP_STOP_MEASUREMENT = 0x03
const MEASUREMENT_ECG = 0x00
const MEASUREMENT_ACC = 0x02
const RECORDING_ONLINE_BIT = 0x00
const CONTROL_POINT_RESPONSE_CODE = 0xf0
const ONLINE_MEASUREMENT_STOPPED = 0x01
const MEASUREMENT_TYPE_MASK = 0x3f
const COMPRESSED_FRAME_BIT = 0x80
const FRAME_TYPE_MASK = 0x7f
const FRAME_HEADER_BYTES = 10
const ECG_TYPE_0_SAMPLE_BYTES = 3

const SETTING_SAMPLE_RATE = 0x00
const SETTING_RESOLUTION = 0x01
const SETTING_RANGE = 0x02

/** Setting id → [name, field size in bytes] (PmdSetting.typeToFieldSize). */
const SETTING_FIELDS: ReadonlyMap<number, readonly [string, number]> = new Map([
  [0, ['SAMPLE_RATE', 2]],
  [1, ['RESOLUTION', 2]],
  [2, ['RANGE', 2]],
  [3, ['RANGE_MILLIUNIT', 4]],
  [4, ['CHANNELS', 1]],
  [5, ['FACTOR', 4]]
])

const RESPONSE_STATUS_NAMES: readonly string[] = [
  'SUCCESS',
  'ERROR_INVALID_OP_CODE',
  'ERROR_INVALID_MEASUREMENT_TYPE',
  'ERROR_NOT_SUPPORTED',
  'ERROR_INVALID_LENGTH',
  'ERROR_INVALID_PARAMETER',
  'ERROR_ALREADY_IN_STATE',
  'ERROR_INVALID_RESOLUTION',
  'ERROR_INVALID_SAMPLE_RATE',
  'ERROR_INVALID_RANGE',
  'ERROR_INVALID_MTU',
  'ERROR_INVALID_NUMBER_OF_CHANNELS',
  'ERROR_INVALID_STATE',
  'ERROR_DEVICE_IN_CHARGER',
  'ERROR_DISK_FULL',
  'ERROR_INVALID_SOURCE_MEASUREMENT_TYPE',
  'ERROR_INVALID_SOURCE_MEASUREMENT_RATE',
  'ERROR_INVALID_DERIVED_MEASUREMENT_SETTINGS_GROUP',
  'ERROR_INVALID_DERIVED_MEASUREMENT_METHOD'
]

export class PmdParseError extends Error {
  readonly code = 'pmd.parse-failed'

  constructor(message: string) {
    super(message)
    this.name = 'PmdParseError'
  }
}

/** `[0x02][ONLINE|ECG][SAMPLE_RATE ×1 = 130][RESOLUTION ×1 = 14]`, uint16 little-endian values. */
export function buildStartEcgCommand(): Uint8Array {
  return new Uint8Array([
    OP_REQUEST_MEASUREMENT_START,
    RECORDING_ONLINE_BIT | MEASUREMENT_ECG,
    SETTING_SAMPLE_RATE,
    1,
    ...uint16le(H10_ECG_SAMPLE_RATE_HZ),
    SETTING_RESOLUTION,
    1,
    ...uint16le(H10_ECG_RESOLUTION_BITS)
  ])
}

export function buildStopEcgCommand(): Uint8Array {
  return new Uint8Array([OP_STOP_MEASUREMENT, MEASUREMENT_ECG])
}

export function buildGetEcgSettingsCommand(): Uint8Array {
  return new Uint8Array([OP_GET_MEASUREMENT_SETTINGS, RECORDING_ONLINE_BIT | MEASUREMENT_ECG])
}

/** H10 supported settings, per PolarH10.md and Polar SDK issue #124. XYZ is fixed, not a CHANNELS selection. */
export function buildStartAccCommand(settings: H10AccSettings): Uint8Array {
  if (
    !H10_ACC_SAMPLE_RATES_HZ.includes(settings.sampleRateHz) ||
    !H10_ACC_RANGES_G.includes(settings.rangeG) ||
    settings.resolutionBits !== H10_ACC_RESOLUTION_BITS
  ) {
    throw new RangeError('H10 ACC requires 25/50/100/200 Hz, 16-bit resolution and ±2/4/8 g range')
  }
  return new Uint8Array([
    OP_REQUEST_MEASUREMENT_START,
    RECORDING_ONLINE_BIT | MEASUREMENT_ACC,
    SETTING_SAMPLE_RATE,
    1,
    ...uint16le(settings.sampleRateHz),
    SETTING_RESOLUTION,
    1,
    ...uint16le(settings.resolutionBits),
    SETTING_RANGE,
    1,
    ...uint16le(settings.rangeG)
  ])
}

export function buildStopAccCommand(): Uint8Array {
  return new Uint8Array([OP_STOP_MEASUREMENT, MEASUREMENT_ACC])
}

export function buildGetAccSettingsCommand(): Uint8Array {
  return new Uint8Array([OP_GET_MEASUREMENT_SETTINGS, RECORDING_ONLINE_BIT | MEASUREMENT_ACC])
}

export type ControlPointMessage =
  | {
      readonly kind: 'response'
      readonly opCode: number
      readonly measurementType: number
      readonly status: number
      readonly statusName: string
      readonly more: boolean
      readonly parameters: Uint8Array
    }
  | { readonly kind: 'online-measurement-stopped'; readonly measurementTypes: readonly number[] }

export function parseControlPointMessage(bytes: Readonly<Uint8Array>): ControlPointMessage {
  if (bytes.length === 0) throw new PmdParseError('empty PMD control point message')
  const first = byteAt(bytes, 0)
  if (first === ONLINE_MEASUREMENT_STOPPED) {
    return {
      kind: 'online-measurement-stopped',
      measurementTypes: [...bytes.slice(1)].map(type => type & MEASUREMENT_TYPE_MASK)
    }
  }
  if (first !== CONTROL_POINT_RESPONSE_CODE) {
    throw new PmdParseError(`unknown PMD control point message 0x${hexByte(first)}`)
  }
  if (bytes.length < 4)
    throw new PmdParseError(`PMD control point response too short (${bytes.length.toString()} bytes)`)
  const status = byteAt(bytes, 3)
  const success = status === 0
  return {
    kind: 'response',
    opCode: byteAt(bytes, 1),
    measurementType: byteAt(bytes, 2),
    status,
    statusName: RESPONSE_STATUS_NAMES[status] ?? 'UNKNOWN_ERROR',
    more: success && bytes.length > 4 && bytes[4] !== 0,
    parameters: success && bytes.length > 5 ? Uint8Array.from(bytes.slice(5)) : new Uint8Array(0)
  }
}

/** One command's bounded multipart reply. Fragments repeat the response header
 * (Polar BlePMDClient.sendPmdCommand); concatenate parameters before TLV parsing.
 * Limits are defensive host bounds, not a claim about peripheral capabilities. */
export class PmdControlPointResponseAssembler {
  private readonly opCode: number
  private readonly measurementType: number
  private readonly generation: number
  private chunks: Uint8Array[] = []
  private bytes = 0
  private fragments = 0
  private complete = false

  constructor(opCode: number, measurementType: number, generation: number) {
    this.opCode = opCode
    this.measurementType = measurementType
    this.generation = generation
  }

  push(message: ControlPointMessage, generation: number): Extract<ControlPointMessage, { kind: 'response' }> | null {
    if (this.complete) throw new PmdParseError('PMD response assembler is already complete')
    if (
      message.kind !== 'response' ||
      message.opCode !== this.opCode ||
      message.measurementType !== this.measurementType ||
      generation !== this.generation
    )
      return null
    this.fragments += 1
    this.bytes += message.parameters.length
    if (this.fragments > 64 || this.bytes > 32768) {
      this.complete = true
      this.chunks = []
      throw new PmdParseError(
        this.fragments > 64 ? 'PMD response fragment limit exceeded' : 'PMD response byte limit exceeded'
      )
    }
    if (message.status !== 0) {
      this.complete = true
      this.chunks = []
      return { ...message, more: false, parameters: new Uint8Array(0) }
    }
    this.chunks.push(Uint8Array.from(message.parameters))
    if (message.more) return null
    this.complete = true
    const parameters = new Uint8Array(this.bytes)
    let offset = 0
    for (const chunk of this.chunks) {
      parameters.set(chunk, offset)
      offset += chunk.length
    }
    this.chunks = []
    return { ...message, more: false, parameters }
  }
}

/** Parses a settings TLV list: `[type][count][count × little-endian field]…`. */
export function parsePmdSettings(bytes: Readonly<Uint8Array>): Readonly<Record<string, readonly number[]>> {
  const settings: Record<string, number[]> = {}
  let offset = 0
  while (offset < bytes.length) {
    const type = byteAt(bytes, offset)
    const field = SETTING_FIELDS.get(type)
    if (field === undefined)
      throw new PmdParseError(`unknown PMD setting type ${type.toString()} at offset ${offset.toString()}`)
    if (offset + 1 >= bytes.length) throw new PmdParseError(`PMD setting ${field[0]} has no count byte`)
    const [name, size] = field
    const count = byteAt(bytes, offset + 1)
    const end = offset + 2 + count * size
    if (end > bytes.length) {
      throw new PmdParseError(
        `PMD setting ${name} declares ${count.toString()} values but only ${(bytes.length - offset - 2).toString()} bytes remain`
      )
    }
    const values: number[] = []
    for (let index = 0; index < count; index += 1) {
      values.push(readUnsignedLe(bytes, offset + 2 + index * size, size))
    }
    settings[name] = [...(settings[name] ?? []), ...values]
    offset = end
  }
  return settings
}

/** Feature read of the control point (PmdMeasurementType.fromByteArray, byte 1 bitmap). */
export function parsePmdFeatures(bytes: Readonly<Uint8Array>): {
  ecg: boolean
  ppg: boolean
  acc: boolean
  ppi: boolean
} {
  if (bytes.length < 2) throw new PmdParseError(`PMD feature read too short (${bytes.length.toString()} bytes)`)
  const bitmap = byteAt(bytes, 1)
  return {
    ecg: (bitmap & 0x01) !== 0,
    ppg: (bitmap & 0x02) !== 0,
    acc: (bitmap & 0x04) !== 0,
    ppi: (bitmap & 0x08) !== 0
  }
}

export interface EcgFrame {
  /** Sensor timestamp of the frame's last sample, nanoseconds (PMD uint64 LE). */
  readonly timestampNs: bigint
  readonly frameType: number
  readonly compressed: boolean
  readonly samplesMicroVolts: readonly number[]
}

function parseFrameHeader(bytes: Readonly<Uint8Array>, expectedType: number, label: string) {
  if (bytes.length < FRAME_HEADER_BYTES) {
    throw new PmdParseError(
      `PMD data frame too short: ${bytes.length.toString()} < ${FRAME_HEADER_BYTES.toString()} bytes`
    )
  }
  const measurementType = byteAt(bytes, 0) & MEASUREMENT_TYPE_MASK
  if (measurementType !== expectedType) {
    throw new PmdParseError(
      `PMD data frame has measurement type ${measurementType.toString()}, expected ${label} (${expectedType.toString()})`
    )
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const timestampNs = view.getBigUint64(1, true)
  const frameTypeByte = byteAt(bytes, 9)
  const compressed = (frameTypeByte & COMPRESSED_FRAME_BIT) !== 0
  const frameType = frameTypeByte & FRAME_TYPE_MASK
  return { timestampNs, frameType, compressed }
}

/** Parses one PMD data notification carrying H10 ECG (raw frame type 0: signed 24-bit LE µV). */
export function parseEcgFrame(bytes: Readonly<Uint8Array>): EcgFrame {
  const { timestampNs, frameType, compressed } = parseFrameHeader(bytes, MEASUREMENT_ECG, 'ECG')
  if (compressed) throw new PmdParseError(`ECG compressed frame type ${frameType.toString()} is not supported`)
  if (frameType !== 0)
    throw new PmdParseError(`ECG raw frame type ${frameType.toString()} is not supported (H10 sends type 0)`)
  const content = bytes.length - FRAME_HEADER_BYTES
  if (content === 0 || content % ECG_TYPE_0_SAMPLE_BYTES !== 0) {
    throw new PmdParseError(`ECG type 0 payload of ${content.toString()} bytes is not a non-zero multiple of 3`)
  }
  const samplesMicroVolts: number[] = []
  for (let offset = FRAME_HEADER_BYTES; offset < bytes.length; offset += ECG_TYPE_0_SAMPLE_BYTES) {
    samplesMicroVolts.push(readSignedLe(bytes, offset, ECG_TYPE_0_SAMPLE_BYTES))
  }
  return { timestampNs, frameType, compressed, samplesMicroVolts }
}

export interface AccSample {
  readonly x: number
  readonly y: number
  readonly z: number
}

export interface AccFrame {
  /** Timestamp of the last sample, nanoseconds since the Polar epoch; never rounded to Number. */
  readonly timestampNs: bigint
  readonly frameType: number
  readonly compressed: false
  /** Signed x/y/z acceleration in milli-g, not raw ADC counts or m/s². */
  readonly samplesMilliG: readonly AccSample[]
}

/**
 * Polar SDK AccData raw formats 0/1/2: signed 8/16/24-bit little-endian XYZ.
 * Format support here does not assert that an H10 advertises every resolution.
 * Compressed data requires separate factor/delta decoding and is refused.
 */
export function parseAccFrame(bytes: Readonly<Uint8Array>): AccFrame {
  const { timestampNs, frameType, compressed } = parseFrameHeader(bytes, MEASUREMENT_ACC, 'ACC')
  if (compressed) throw new PmdParseError(`ACC compressed frame type ${frameType.toString()} is not supported`)
  if (frameType > 2) throw new PmdParseError(`ACC raw frame type ${frameType.toString()} is not supported`)
  const axisBytes = frameType + 1
  const sampleBytes = axisBytes * 3
  const content = bytes.length - FRAME_HEADER_BYTES
  if (content === 0 || content % sampleBytes !== 0) {
    throw new PmdParseError(
      `ACC type ${frameType.toString()} payload of ${content.toString()} bytes is not a non-zero multiple of ${sampleBytes.toString()}`
    )
  }
  const samplesMilliG: AccSample[] = []
  for (let offset = FRAME_HEADER_BYTES; offset < bytes.length; offset += sampleBytes) {
    samplesMilliG.push({
      x: readSignedLe(bytes, offset, axisBytes),
      y: readSignedLe(bytes, offset + axisBytes, axisBytes),
      z: readSignedLe(bytes, offset + axisBytes * 2, axisBytes)
    })
  }
  return { timestampNs, frameType, compressed: false, samplesMilliG }
}

function readSignedLe(bytes: Readonly<Uint8Array>, offset: number, size: number): number {
  const unsigned = readUnsignedLe(bytes, offset, size)
  const modulus = 2 ** (size * 8)
  return unsigned >= modulus / 2 ? unsigned - modulus : unsigned
}

export interface EcgFrameFindings {
  readonly timestampGap: {
    readonly deltaNs: number
    readonly expectedNs: number
    readonly estimatedMissingSamples: number
  } | null
  readonly sequenceGap: { readonly from: number; readonly to: number; readonly missingNotifications: number } | null
}

export interface EcgStreamSummary {
  readonly frames: number
  readonly samples: number
  readonly samplesLastSecond: number
  readonly sensorSampleRateHz: number | null
  readonly timestampGaps: number
  readonly estimatedMissingSamples: number
  readonly missingNotifications: number
  readonly lastSampleMicroVolts: number | null
}

const GAP_TOLERANCE = 1.5
const RATE_WINDOW_MS = 1_000

/** Accounting over parsed frames: throughput, sensor-clock gaps and notification-sequence drops. */
export class EcgStreamStats {
  private readonly nominalRateHz: number
  private frames = 0
  private samples = 0
  private samplesAfterFirstFrame = 0
  private firstTimestampNs: bigint | null = null
  private lastTimestampNs: bigint | null = null
  private lastSequence: number | null = null
  private timestampGaps = 0
  private estimatedMissingSamples = 0
  private missingNotifications = 0
  private lastSample: number | null = null
  private receipts: { readonly atMs: number; readonly samples: number }[] = []

  constructor(nominalRateHz: number) {
    this.nominalRateHz = nominalRateHz
  }

  record(frame: EcgFrame, receivedAtMs: number, notificationSequence: number): EcgFrameFindings {
    const count = frame.samplesMicroVolts.length
    let timestampGap: EcgFrameFindings['timestampGap'] = null
    if (this.lastTimestampNs !== null) {
      const deltaNs = Number(frame.timestampNs - this.lastTimestampNs)
      const expectedNs = (count * 1e9) / this.nominalRateHz
      if (deltaNs > expectedNs * GAP_TOLERANCE) {
        const estimatedMissingSamples = Math.max(0, Math.round((deltaNs * this.nominalRateHz) / 1e9) - count)
        timestampGap = { deltaNs, expectedNs, estimatedMissingSamples }
        this.timestampGaps += 1
        this.estimatedMissingSamples += estimatedMissingSamples
      }
      this.samplesAfterFirstFrame += count
    } else {
      this.firstTimestampNs = frame.timestampNs
    }
    let sequenceGap: EcgFrameFindings['sequenceGap'] = null
    if (this.lastSequence !== null && notificationSequence > this.lastSequence + 1) {
      const missingNotifications = notificationSequence - this.lastSequence - 1
      sequenceGap = { from: this.lastSequence, to: notificationSequence, missingNotifications }
      this.missingNotifications += missingNotifications
    }
    this.lastSequence = notificationSequence
    this.lastTimestampNs = frame.timestampNs
    this.frames += 1
    this.samples += count
    this.lastSample = frame.samplesMicroVolts[count - 1] ?? this.lastSample
    this.receipts = [
      ...this.receipts.filter(receipt => receipt.atMs > receivedAtMs - RATE_WINDOW_MS),
      { atMs: receivedAtMs, samples: count }
    ]
    return { timestampGap, sequenceGap }
  }

  summary(nowMs: number): EcgStreamSummary {
    const spanNs =
      this.firstTimestampNs === null || this.lastTimestampNs === null
        ? 0
        : Number(this.lastTimestampNs - this.firstTimestampNs)
    return {
      frames: this.frames,
      samples: this.samples,
      samplesLastSecond: this.receipts
        .filter(receipt => receipt.atMs > nowMs - RATE_WINDOW_MS)
        .reduce((total, receipt) => total + receipt.samples, 0),
      sensorSampleRateHz: spanNs > 0 ? (this.samplesAfterFirstFrame * 1e9) / spanNs : null,
      timestampGaps: this.timestampGaps,
      estimatedMissingSamples: this.estimatedMissingSamples,
      missingNotifications: this.missingNotifications,
      lastSampleMicroVolts: this.lastSample
    }
  }
}

function uint16le(value: number): [number, number] {
  return [value & 0xff, (value >> 8) & 0xff]
}

function readUnsignedLe(bytes: Readonly<Uint8Array>, offset: number, size: number): number {
  let value = 0
  for (let index = size - 1; index >= 0; index -= 1) value = value * 256 + byteAt(bytes, offset + index)
  return value
}

/** A bounds-checked byte read: a short buffer is a parse failure, never `undefined` arithmetic. */
export function byteAt(bytes: Readonly<Uint8Array>, offset: number): number {
  const value = bytes[offset]
  if (value === undefined)
    throw new PmdParseError(
      `PMD message of ${bytes.length.toString()} bytes has no byte at offset ${offset.toString()}`
    )
  return value
}

function hexByte(value: number): string {
  return value.toString(16).padStart(2, '0')
}
