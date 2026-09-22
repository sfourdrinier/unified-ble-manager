// __tests__/profiles/profile-codecs.test.js
//
// Fixed-vector coverage for shipped SIG codec helpers without a dedicated
// backend round-trip (DATA-03). Vectors mirror the Rust profiles module so
// oracle parity is checked on both sides of the facade.

const {
  encodeBatteryLevel,
  parseBatteryLevel,
  batteryLevelSelector
} = require('../../src/profiles/battery-service')
const {
  encodeResetEnergyExpended,
  parseBodySensorLocation,
  parseHeartRateMeasurement
} = require('../../src/profiles/heart-rate')
const {
  BLOOD_PRESSURE_SERVICE,
  BODY_SENSOR_LOCATION_CHARACTERISTIC,
  HEART_RATE_SERVICE
} = require('../../src/profiles/identifiers')
const {
  decodeIeee11073Float,
  decodeIeee11073Sfloat,
  encodeIeee11073Float,
  encodeIeee11073Sfloat
} = require('../../src/profiles/ieee-11073')
const {
  decodeDeviceInformationString,
  parsePnpId,
  parseSystemId
} = require('../../src/profiles/device-information')
const { parseTemperatureMeasurement } = require('../../src/profiles/health-thermometer')
const { parseBloodPressureMeasurement } = require('../../src/profiles/blood-pressure')

function capturedError(operation) {
  try {
    operation()
  } catch (error) {
    return error
  }
  throw new Error('expected operation to throw')
}

describe('profile codec fixed vectors', () => {
  test('battery level round-trips and rejects out-of-range values', () => {
    expect(parseBatteryLevel(new Uint8Array([80]))).toBe(80)
    expect(encodeBatteryLevel(80)).toEqual(new Uint8Array([80]))
    expect(capturedError(() => encodeBatteryLevel(101))).toMatchObject({
      code: 'profile.codec.invalid-value'
    })
    expect(capturedError(() => encodeBatteryLevel(-1))).toMatchObject({
      code: 'profile.codec.invalid-value'
    })
  })

  test('heart-rate control point and sensor location keep exact wire values', () => {
    expect(encodeResetEnergyExpended()).toEqual(new Uint8Array([0x01]))
    expect(parseBodySensorLocation(new Uint8Array([1]))).toBe(1)
    expect(capturedError(() => parseBodySensorLocation(new Uint8Array([7])))).toMatchObject({
      code: 'profile.codec.reserved'
    })
    expect(capturedError(() => parseBodySensorLocation(new Uint8Array([1, 2])))).toMatchObject({
      code: 'profile.codec.malformed'
    })
    expect(parseHeartRateMeasurement(new Uint8Array([0x06, 72]))).toMatchObject({
      beatsPerMinute: 72,
      contact: 'detected',
      energyExpendedKilojoules: null,
      rrIntervalsSeconds: []
    })
  })

  test('ieee-11073 codecs round-trip finite and special values', () => {
    const finite = { kind: 'finite', mantissa: 366, exponent: -1, value: 36.6 }
    const sfloatBytes = encodeIeee11073Sfloat(finite)
    expect(sfloatBytes).toEqual(new Uint8Array([0x6e, 0xf1]))
    expect(decodeIeee11073Sfloat(sfloatBytes)).toMatchObject({ kind: 'finite', mantissa: 366, exponent: -1 })
    const floatBytes = encodeIeee11073Float(finite)
    expect(floatBytes).toEqual(new Uint8Array([0x6e, 0x01, 0x00, 0xff]))
    expect(decodeIeee11073Float(floatBytes)).toMatchObject({ kind: 'finite', mantissa: 366, exponent: -1 })
    expect(encodeIeee11073Sfloat({ kind: 'nan' })).toEqual(new Uint8Array([0xff, 0x07]))
    expect(decodeIeee11073Sfloat(new Uint8Array([0xff, 0x07]))).toEqual({ kind: 'nan' })
    expect(capturedError(() => encodeIeee11073Sfloat({ ...finite, mantissa: 2048 }))).toMatchObject({
      code: 'profile.codec.invalid-value'
    })
    expect(capturedError(() => encodeIeee11073Float({ ...finite, exponent: 128 }))).toMatchObject({
      code: 'profile.codec.invalid-value'
    })
  })

  test('device information strings, system id and pnp id decode', () => {
    expect(decodeDeviceInformationString(new TextEncoder().encode('ACME'))).toBe('ACME')
    expect(capturedError(() => decodeDeviceInformationString(new Uint8Array([0xff, 0xfe])))).toMatchObject({
      code: 'profile.codec.malformed'
    })
    expect(
      parseSystemId(new Uint8Array([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]))
    ).toEqual({
      manufacturerIdentifier: 0x0504030201n,
      organizationallyUniqueIdentifier: 0x080706
    })
    expect(parsePnpId(new Uint8Array([0x01, 0x34, 0x12, 0x78, 0x56, 0x01, 0x00]))).toEqual({
      vendorIdSource: 'bluetooth-sig',
      vendorId: 0x1234,
      productId: 0x5678,
      productVersion: 0x0001
    })
    expect(capturedError(() => parsePnpId(new Uint8Array([0x03, 0, 0, 0, 0, 0, 0])))).toMatchObject({
      code: 'profile.codec.reserved'
    })
  })

  test('temperature measurement keeps timestamp and type absence exactly', () => {
    expect(
      parseTemperatureMeasurement(new Uint8Array([0x07, 0xda, 0x03, 0x00, 0xff, 0xe8, 0x07, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]))
    ).toMatchObject({
      unit: 'fahrenheit',
      temperature: { kind: 'finite', mantissa: 986, exponent: -1 },
      timestamp: { year: 2024, month: 1, day: 2, hours: 3, minutes: 4, seconds: 5 },
      type: 'mouth'
    })
    expect(
      capturedError(
        () =>
          parseTemperatureMeasurement(new Uint8Array([0x04, 0x6e, 0x01, 0x00, 0xff, 0x02, 0x00]))
      )
    ).toMatchObject({ code: 'profile.codec.malformed' })
  })

  test('blood pressure measurement keeps optional fields and unknown user', () => {
    expect(
      parseBloodPressureMeasurement(
        new Uint8Array([
          0x1e, 0x78, 0x00, 0x50, 0x00, 0x5d, 0x00, 0xe8, 0x07, 0x05, 0x06, 0x0c, 0x1e, 0x2d,
          0x46, 0x00, 0xff, 0x03, 0x00
        ])
      )
    ).toEqual({
      unit: 'millimetres-of-mercury',
      systolic: { kind: 'finite', mantissa: 120, exponent: 0, value: 120 },
      diastolic: { kind: 'finite', mantissa: 80, exponent: 0, value: 80 },
      meanArterialPressure: { kind: 'finite', mantissa: 93, exponent: 0, value: 93 },
      timestamp: { year: 2024, month: 5, day: 6, hours: 12, minutes: 30, seconds: 45 },
      pulseRate: { kind: 'finite', mantissa: 70, exponent: 0, value: 70 },
      userId: 255,
      userIdIsUnknown: true,
      measurementStatus: 3
    })
    expect(
      capturedError(
        () =>
          parseBloodPressureMeasurement(
            new Uint8Array([0x10, 0x78, 0x00, 0x50, 0x00, 0x5d, 0x00, 0x00, 0xc0])
          )
      )
    ).toMatchObject({ code: 'profile.codec.reserved' })
  })

  test('selectors default to uuid-only with explicit occurrence opt-in', () => {
    expect(batteryLevelSelector()).toMatchObject({
      serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
      characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
      serviceOccurrence: null,
      characteristicOccurrence: null
    })
    expect(HEART_RATE_SERVICE).toBe('0000180d-0000-1000-8000-00805f9b34fb')
    expect(BLOOD_PRESSURE_SERVICE).toBe('00001810-0000-1000-8000-00805f9b34fb')
    expect(BODY_SENSOR_LOCATION_CHARACTERISTIC).toBe('00002a38-0000-1000-8000-00805f9b34fb')
  })
})
