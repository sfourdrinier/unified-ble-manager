const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')
const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')
const {
  field,
  protocolRecord,
  requiredRecord,
  requiredString
} = require('../../src/native-protocol/rn-android-protocol-records')
const { planReactNativeAndroidScan } = require('../../src/backends/reactnative/react-native-scan-planner')
const {
  normalizeScanQuery,
  normalizeScanObservation,
  observationMatchesScanQuery
} = require('../../src/public/scan-query')
const { deviceIdentity } = require('../../src/backend-contract/advertisement')
const { canonicalBleAddress, opaqueId } = require('../../src/backend-contract/primitives')
const {
  createCoreBluetoothObservation
} = require('../../src/backends/corebluetooth/corebluetooth-advertisement-observation')
const { ReactNativeAppleProtocolBoundary } = require('../../src/native-protocol/rn-apple-boundary')

const attachment = {
  attachmentId: 'android-scan-address-attachment',
  backendInstanceId: 'android-scan-address-backend',
  backendGeneration: 'android-scan-address-generation',
  adapterId: 'android-scan-address-adapter',
  adapterGeneration: 'android-scan-address-adapter-generation'
}

describe('React Native Android ScanFilter device address', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
  })

  afterEach(() => {
    if (previousRuntime === undefined) delete global.__unifiedBleNativeProtocolV2
    else global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test('encodes scan clause addresses onto native startScan for ScanFilter.setDeviceAddress', async () => {
    const control = new ScanAddressControl()
    const runtime = new ScanAddressRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-scan-address-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await boundary.startScan(() => undefined, [], ['98:75:96:A2:14:34'])

    expect(runtime.deviceAddresses).toEqual(['98:75:96:A2:14:34'])

    await boundary.destroy()
  })

  test('android scan planner pushes address predicates into the native filter', () => {
    const plan = planReactNativeAndroidScan(normalizeScanQuery({ anyOf: [{ addresses: ['98:75:96:A2:14:34'] }] }))
    expect(plan.nativeFilter.deviceAddresses).toEqual(['98:75:96:A2:14:34'])
    expect(plan.native.predicates.some(predicate => predicate.field === 'addresses')).toBe(true)
  })

  test('android MAC nativePeerId observations match an addresses scan clause', () => {
    const mac = '98:75:96:A2:14:34'
    const observation = createCoreBluetoothObservation(
      { nativePeerId: mac, localName: null, rssi: -50, serviceUuids: null },
      deviceIdentity('peer-1', 'backend-1', { value: canonicalBleAddress(mac), type: 'public' }),
      opaqueId('scan-1', 'scan-session', 'android'),
      1,
      0
    )
    const query = normalizeScanQuery({ anyOf: [{ addresses: [mac] }] })
    expect(observationMatchesScanQuery(query, normalizeScanObservation(observation))).toBe(true)
  })

  test('apple startScan fails closed when device addresses are provided', async () => {
    const control = new ScanAddressControl()
    const runtime = new ScanAddressRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAppleProtocolBoundary(control, 'apple-scan-address-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await expect(boundary.startScan(() => undefined, [], ['98:75:96:A2:14:34'])).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported', operation: 'rn-apple-boundary.scan.device-addresses' }
    })
    expect(runtime.deviceAddresses).toBe(null)

    await boundary.destroy()
  })
})

class ScanAddressControl {
  handshake() {
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 3,
      controlSurface: 2,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 65536,
      maximumBinaryPayloadBytes: 524288
    })
  }

  installExecutionRuntime() {
    return Promise.resolve()
  }

  closeAttachment() {
    return Promise.resolve()
  }
}

class ScanAddressRuntime {
  constructor() {
    this.listener = null
    this.deviceAddresses = null
  }

  setEventSink(listener) {
    this.listener = listener
  }

  setFatalSink() {}

  submit(bytes) {
    const command = decodeNativeProtocolRecord(bytes)
    const kind = requiredString(command, 3, 'test.command.kind')
    if (kind === 'scanStart') {
      const options = requiredRecord(command, 12, 'test.scan-options')
      const addresses = options.fields.find(entry => entry.id === 6)?.value
      this.deviceAddresses = Array.isArray(addresses) ? [...addresses] : []
      this.emitResult(command, 'scanStarted')
      return
    }
    if (kind === 'destroy') {
      this.emitResult(command, 'destroyed')
      return
    }
    throw new Error(`Unexpected test command ${kind}`)
  }

  emitResult(command, kind) {
    this.emit(
      protocolRecord('result', [
        field(1, 1),
        field(2, kind),
        field(
          3,
          protocolRecord('terminal', [field(1, requiredRecord(command, 2, 'test.correlation')), field(2, 'succeeded')])
        )
      ])
    )
  }

  emit(value) {
    if (this.listener === null) throw new Error('The scan-address test runtime has no event sink')
    this.listener(encodeNativeProtocolRecord(value))
  }
}
