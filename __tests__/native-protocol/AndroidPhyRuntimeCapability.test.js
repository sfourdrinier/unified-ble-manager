const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')
const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')
const {
  field,
  protocolRecord,
  requiredRecord,
  requiredString
} = require('../../src/native-protocol/rn-android-protocol-records')

const attachment = {
  attachmentId: 'android-phy-capability-attachment',
  backendInstanceId: 'android-phy-capability-backend',
  backendGeneration: 'android-phy-capability-generation',
  adapterId: 'android-phy-capability-adapter',
  adapterGeneration: 'android-phy-capability-adapter-generation'
}
const peerId = 'C0FFEE000001'

describe('React Native Android PHY runtime capability', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
  })

  afterEach(() => {
    if (previousRuntime === undefined) delete global.__unifiedBleNativeProtocolV2
    else global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test.each([
    { label: 'false on API 24', capability: false },
    { label: 'false on API 25', capability: false },
    { label: 'absent', capability: 'absent' }
  ])('fails closed when the native PHY handshake capability is $label', async ({ capability }) => {
    const control = new PhyCapabilityControl(capability)
    const runtime = new PhyCapabilityRuntime(control)
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)
    await boundary.connect(peerId)

    expect(boundary.connectionControlCapabilities.phy).toBe('unavailable')
    await expect(boundary.readPhy(peerId)).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    await expect(boundary.requestPhy(peerId, { tx: 'le-2m' })).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    expect(runtime.commands).toEqual(['connect'])

    await boundary.destroy()
  })

  test('preserves PHY dispatch when the native handshake capability is true', async () => {
    const control = new PhyCapabilityControl(true)
    const runtime = new PhyCapabilityRuntime(control)
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)
    await boundary.connect(peerId)

    expect(boundary.connectionControlCapabilities.phy).toBe('available')
    await expect(boundary.readPhy(peerId)).resolves.toEqual({ txPhy: 'le-2m', rxPhy: 'le-coded' })
    await expect(boundary.requestPhy(peerId, { tx: 'le-2m', rx: 'le-coded' })).resolves.toEqual({
      accepted: true,
      observation: { txPhy: 'le-2m', rxPhy: 'le-coded' }
    })
    expect(runtime.commands).toEqual(['connect', 'readPhy', 'requestPhy'])

    await boundary.destroy()
  })
})

async function openBoundary(control) {
  const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-phy-capability-owner')
  boundary.bindAttachment(attachment)
  await boundary.open()
  return boundary
}

class PhyCapabilityControl {
  constructor(capability) {
    this.capability = capability
    this.handshakes = []
  }

  handshake(request) {
    this.handshakes.push(request)
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 3,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 65536,
      maximumBinaryPayloadBytes: 524288,
      ...(this.capability === 'absent' ? {} : { phyAvailable: this.capability })
    })
  }

  installExecutionRuntime() {
    return Promise.resolve()
  }

  closeAttachment() {
    return Promise.resolve()
  }
}

class PhyCapabilityRuntime {
  constructor(control) {
    this.control = control
    this.commands = []
    this.listener = null
  }

  setEventSink(listener) {
    this.listener = listener
  }

  setFatalSink() {}

  submit(bytes) {
    const command = decodeNativeProtocolRecord(bytes)
    const kind = requiredString(command, 3, 'test.command.kind')
    this.commands.push(kind)
    if (kind === 'connect') {
      this.emitResult(command, 'connected', [field(11, requiredRecord(command, 10, 'test.connect'))])
      return
    }
    if (kind === 'readPhy') {
      this.emitResult(command, 'phy', [field(19, 'le2m'), field(20, 'leCoded')])
      return
    }
    if (kind === 'requestPhy') {
      this.emitResult(command, 'phy', [field(19, 'le2m'), field(20, 'leCoded'), field(21, true)])
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
    if (this.listener === null) throw new Error('The PHY capability test runtime has no event sink')
    this.listener(encodeNativeProtocolRecord(value))
  }
}
