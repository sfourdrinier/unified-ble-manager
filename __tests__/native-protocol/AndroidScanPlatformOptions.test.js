const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')
const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')
const {
  field,
  protocolRecord,
  requiredBoolean,
  requiredRecord,
  requiredSigned,
  requiredString
} = require('../../src/native-protocol/rn-android-protocol-records')

const attachment = {
  attachmentId: 'android-scan-platform-attachment',
  backendInstanceId: 'android-scan-platform-backend',
  backendGeneration: 'android-scan-platform-generation',
  adapterId: 'android-scan-platform-adapter',
  adapterGeneration: 'android-scan-platform-adapter-generation'
}

describe('React Native Android scan platform options', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
  })

  afterEach(() => {
    if (previousRuntime === undefined) delete global.__unifiedBleNativeProtocolV2
    else global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test('encodes android low-power scan mode instead of the hardcoded low-latency default', async () => {
    const control = new ScanPlatformControl()
    const runtime = new ScanPlatformRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-scan-platform-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await boundary.startScan(() => undefined, ['0000180d-0000-1000-8000-00805f9b34fb'], [], {
      kind: 'android',
      mode: 'low-power',
      callbackType: 'first-match',
      legacy: false
    })

    expect(runtime.scanOptions).toEqual({
      scanMode: 0,
      callbackType: 2,
      legacyScan: false
    })

    await boundary.destroy()
  })

  test('fails closed when android platform options include fields the native record cannot encode', async () => {
    const control = new ScanPlatformControl()
    const runtime = new ScanPlatformRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-scan-platform-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await expect(boundary.startScan(() => undefined, [], [], { kind: 'android', phy: '1m' })).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    await expect(
      boundary.startScan(() => undefined, [], [], { kind: 'android', reportDelayMs: 100 })
    ).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
    expect(runtime.scanOptions).toBe(null)

    await expect(boundary.startScan(() => undefined, [], [], { kind: 'corebluetooth' })).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })

    await boundary.destroy()
  })

  test('encodes the opportunistic scan mode as -1 for the ABI-4 native radio', async () => {
    const control = new ScanPlatformControl()
    const runtime = new ScanPlatformRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-scan-platform-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await boundary.startScan(() => undefined, [], [], { kind: 'android', mode: 'opportunistic' })
    expect(runtime.scanOptions).toEqual({ scanMode: -1, callbackType: 1, legacyScan: true })

    await boundary.destroy()
  })

  test('fails closed on match-lost callback type instead of encoding it', async () => {
    // The radio forwards every scan callback through the ordinary
    // advertisement path, so a MATCH_LOST callback would surface as a fresh
    // observation. Until the native event protocol can represent a loss, the
    // boundary must not encode the request.
    const control = new ScanPlatformControl()
    const runtime = new ScanPlatformRuntime()
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-scan-platform-owner')
    boundary.bindAttachment(attachment)
    await boundary.open()

    await expect(
      boundary.startScan(() => undefined, [], [], { kind: 'android', callbackType: 'match-lost' })
    ).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported', operation: 'rn-android-boundary.scan.callback-type-match-lost' }
    })
    expect(runtime.scanOptions).toBe(null)

    await boundary.destroy()
  })
})

describe('OwnedAndroidGattRadio scan platform source guards', () => {
  const fs = require('fs')
  const path = require('path')
  const radio = fs.readFileSync(
    path.resolve(__dirname, '../../android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'),
    'utf8'
  )
  const startScan = radio.slice(radio.indexOf('fun startScan('), radio.indexOf('internal fun stopScan()'))

  test('maps the versioned -1 scan mode to SCAN_MODE_OPPORTUNISTIC', () => {
    expect(startScan).toMatch(/-1\s*->\s*ScanSettings\.SCAN_MODE_OPPORTUNISTIC/)
  })

  test('rejects legacyScan=false before any scan state changes on pre-26 devices', () => {
    // API 24-25 devices have no ScanSettings.Builder.setLegacy; accepting the
    // option there would start an unchanged legacy scan and report success.
    const guard = startScan.indexOf('require(legacyScan || Build.VERSION.SDK_INT >= 26)')
    expect(guard).toBeGreaterThan(-1)
    expect(guard).toBeLessThan(startScan.indexOf('scanSeenDeviceIds.clear()'))
    expect(guard).toBeLessThan(startScan.indexOf('check(scanCallback == null)'))
    // setLegacy stays gated to API 26+ where it is honoured.
    expect(startScan).toMatch(/if \(Build\.VERSION\.SDK_INT >= 26\) \{\s*\n\s*builder\.setLegacy\(legacyScan\)/)
  })
})

class ScanPlatformControl {
  handshake() {
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 4,
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

class ScanPlatformRuntime {
  constructor() {
    this.listener = null
    this.scanOptions = null
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
      this.scanOptions = {
        scanMode: requiredSigned(options, 3, 'test.scan-options.scan-mode'),
        callbackType: requiredSigned(options, 4, 'test.scan-options.callback-type'),
        legacyScan: requiredBoolean(options, 5, 'test.scan-options.legacy')
      }
      this.emitResult(command, 'scanStarted')
      return
    }
    if (kind === 'destroy') {
      this.emitResult(command, 'destroyed')
      return
    }
    throw new Error(`Unexpected test command ${kind}`)
  }

  emitResult(command, kind, additions = []) {
    this.emit(
      protocolRecord('result', [
        field(1, 1),
        field(2, kind),
        field(
          3,
          protocolRecord('terminal', [field(1, requiredRecord(command, 2, 'test.correlation')), field(2, 'succeeded')])
        ),
        ...additions
      ])
    )
  }

  emit(value) {
    if (this.listener === null) throw new Error('The scan-platform test runtime has no event sink')
    this.listener(encodeNativeProtocolRecord(value))
  }
}
