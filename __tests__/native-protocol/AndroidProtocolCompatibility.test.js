const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')

const attachment = {
  attachmentId: 'android-compatibility-attachment',
  backendInstanceId: 'android-compatibility-backend',
  backendGeneration: 'android-compatibility-generation',
  adapterId: 'android-compatibility-adapter',
  adapterGeneration: 'android-compatibility-adapter-generation'
}

describe('React Native Android native protocol compatibility', () => {
  test('rejects an installed ABI-v2 binary before runtime installation or command dispatch', async () => {
    const control = new LegacyAbiV2Control()
    const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-compatibility-owner')
    boundary.bindAttachment(attachment)

    await expect(boundary.open()).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible' }
    })
    expect(control.handshakes[0].abi).toEqual({ minimum: 6, maximum: 6 })
    expect(control.installCalls).toBe(0)
    expect(control.closedAttachments).toEqual([attachment])
  })
})

class LegacyAbiV2Control {
  constructor() {
    this.handshakes = []
    this.installCalls = 0
    this.closedAttachments = []
  }

  handshake(request) {
    this.handshakes.push(request)
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 2,
      controlSurface: 2,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 262144,
      maximumBinaryPayloadBytes: 524288
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
