const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')

const attachment = {
  attachmentId: 'control-surface-compatibility-attachment',
  backendInstanceId: 'control-surface-compatibility-backend',
  backendGeneration: 'control-surface-compatibility-generation',
  adapterId: 'control-surface-compatibility-adapter',
  adapterGeneration: 'control-surface-compatibility-adapter-generation'
}

describe('React Native native control-surface compatibility', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
    global.__unifiedBleNativeProtocolV2 = {
      setFatalSink() {},
      setEventSink() {}
    }
  })

  afterEach(() => {
    if (previousRuntime === undefined) delete global.__unifiedBleNativeProtocolV2
    else global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test.each([
    ['missing', undefined],
    ['legacy v1', 1]
  ])('rejects a v2 native result with a %s control-surface version before runtime installation', async (_label, version) => {
    const control = new LegacyControlSurfaceControl(version)
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'control-surface-compatibility-owner')
    boundary.bindAttachment(attachment)

    await expect(boundary.open()).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible' }
    })
    expect(control.handshakes[0].controlSurface).toEqual({ minimum: 2, maximum: 2 })
    expect(control.installCalls).toBe(0)
    expect(control.closedAttachments).toEqual([attachment])
  })
})

class LegacyControlSurfaceControl {
  constructor(controlSurface) {
    this.controlSurface = controlSurface
    this.handshakes = []
    this.installCalls = 0
    this.closedAttachments = []
  }

  handshake(request) {
    this.handshakes.push(request)
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 7,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 262144,
      maximumBinaryPayloadBytes: 524288,
      ...(this.controlSurface === undefined ? {} : { controlSurface: this.controlSurface })
    })
  }

  installExecutionRuntime() {
    this.installCalls += 1
    return Promise.resolve()
  }

  closeAttachment(attachmentValue) {
    this.closedAttachments.push(attachmentValue)
    return Promise.resolve()
  }
}
