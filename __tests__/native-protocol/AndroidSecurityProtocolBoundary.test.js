const { contractError } = require('../../src/backend-contract/errors')
const {
  createReactNativeAndroidBackendProvider,
  reactNativeAndroidDefaultAdapterId
} = require('../../src/backends/reactnative/react-native-android-provider')
const { ReactNativeAndroidSecurityBackend } = require('../../src/backends/reactnative/react-native-android-security')
const { ReactNativeAndroidProtocolBoundary } = require('../../src/native-protocol/rn-android-boundary')
const { decodeNativeProtocolRecord, encodeNativeProtocolRecord } = require('../../src/native-protocol/v2-codec')
const {
  field,
  protocolRecord,
  requiredRecord,
  requiredString
} = require('../../src/native-protocol/rn-android-protocol-records')

const attachment = {
  attachmentId: 'android-security-attachment',
  backendInstanceId: 'android-security-backend',
  backendGeneration: 'android-security-generation',
  adapterId: 'android-security-adapter',
  adapterGeneration: 'android-security-adapter-generation'
}
const peerId = 'C0FFEE000001'

describe('React Native Android security protocol boundary', () => {
  let previousRuntime

  beforeEach(() => {
    previousRuntime = global.__unifiedBleNativeProtocolV2
  })

  afterEach(() => {
    if (previousRuntime === undefined) delete global.__unifiedBleNativeProtocolV2
    else global.__unifiedBleNativeProtocolV2 = previousRuntime
  })

  test('fails closed when the native handshake does not advertise the security extension', async () => {
    const control = new SecurityControl(false)
    const runtime = new SecurityRuntime(control)
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)

    expect(boundary.securityAvailable).toBe(false)
    await expect(boundary.securityState(peerId)).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    expect(runtime.commands).toEqual([])

    await boundary.destroy()
  })

  test('round-trips system pairing, cancellation, and authoritative bond-state events', async () => {
    const control = new SecurityControl(true, true)
    const runtime = new SecurityRuntime(control)
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)
    const events = []
    const removeListener = boundary.onSecurityState(event => events.push(event))

    await expect(boundary.securityState(peerId)).resolves.toMatchObject({
      bond: 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: true
    })
    await expect(boundary.pair(peerId)).resolves.toMatchObject({
      outcome: 'paired',
      state: { bond: 'bonded' }
    })

    runtime.emitSecurityState(peerId, 'bonded')
    await boundary.cancelPairing(peerId)

    expect(runtime.commands).toEqual(['securityState', 'securityState', 'securityPair', 'securityCancelPairing'])
    expect(events).toEqual([
      expect.objectContaining({ nativePeerId: peerId, state: expect.objectContaining({ bond: 'bonded' }) })
    ])

    removeListener()
    await boundary.destroy()
  })

  test('does not infer pairing availability from an unknown bond state', async () => {
    const control = new SecurityControl(true, true)
    const runtime = new SecurityRuntime(control)
    runtime.bondState = 'unknown'
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)

    await expect(boundary.securityState(peerId)).resolves.toMatchObject({
      bond: 'unknown',
      pairingPossible: null
    })
    await boundary.destroy()
  })

  test('quarantines a security event from a stale attachment generation', async () => {
    const control = new SecurityControl(true, true)
    const runtime = new SecurityRuntime(control)
    runtime.eventAttachment = protocolRecord('attachment', [
      field(1, 'stale-attachment'),
      field(2, attachment.backendInstanceId),
      field(3, 'stale-generation'),
      field(4, attachment.adapterId),
      field(5, attachment.adapterGeneration)
    ])
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)
    const events = []
    const removeListener = boundary.onSecurityState(event => events.push(event))

    runtime.emitSecurityState(peerId, 'bonded')

    expect(events).toEqual([])
    expectConsoleErrorMatching(
      '[ReactNativeAndroidProtocolBoundary.receiveRecord] Native record was rejected:',
      expect.objectContaining({
        normalized: expect.objectContaining({
          operation: 'rn-android-boundary.security-event.attachment-mismatch'
        })
      })
    )
    removeListener()
    await boundary.destroy()
  })

  test('uses cancellation commands as cleanup-only ownership release when physical cancellation is unavailable', async () => {
    const control = new SecurityControl(true, false)
    const runtime = new SecurityRuntime(control)
    runtime.deferPair = true
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)

    const pairing = boundary.pair(peerId)
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve()
    await boundary.cleanupPairing(peerId)

    await expect(pairing).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(runtime.commands).toEqual(['securityState', 'securityPair', 'securityCancelPairing'])
    await boundary.destroy()
  })

  test('rejects a native result that changes the requested peer identity', async () => {
    const control = new SecurityControl(true, true)
    const runtime = new SecurityRuntime(control)
    runtime.resultPeerId = 'DIFFERENT-PEER'
    global.__unifiedBleNativeProtocolV2 = runtime
    const boundary = await openBoundary(control)

    await expect(boundary.securityState(peerId)).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
    await boundary.destroy()
  })

  test('does not register unpair when the Android platform boundary is explicitly unsupported', async () => {
    const control = new SecurityControl(true, false)
    const runtime = new SecurityRuntime(control)
    global.__unifiedBleNativeProtocolV2 = runtime
    const provider = createReactNativeAndroidBackendProvider({
      control,
      now: () => 20,
      createOwnerId: () => 'android-security-provider-owner'
    })

    const backend = await provider.create({ selectedAdapterId: reactNativeAndroidDefaultAdapterId() })
    expect(backend.features.descriptors).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ id: 'security:state', state: 'limited' }),
        expect.objectContaining({ id: 'security:pair', state: 'limited' })
      ])
    )
    expect(backend.features.descriptors).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ id: 'security:cancel-pairing' })])
    )
    expect(backend.features.descriptors).not.toEqual(
      expect.arrayContaining([expect.objectContaining({ id: 'security:unpair' })])
    )

    await backend.destroy()
  })

  test('rejects an already-aborted or already-expired pair before allocating native work', async () => {
    const pair = jest.fn(async () => ({ outcome: 'paired', state: securityState('bonded') }))
    const security = new ReactNativeAndroidSecurityBackend(securityAdapter({ pair }), () => 20)

    const controller = new AbortController()
    controller.abort()
    await expect(
      security.pair(peerId, {
        signal: controller.signal,
        deadline: null,
        transport: 'auto',
        protection: 'system-default',
        ceremony: 'system'
      })
    ).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(
      security.pair(peerId, {
        signal: null,
        deadline: 10,
        transport: 'auto',
        protection: 'system-default',
        ceremony: 'system'
      })
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(pair).not.toHaveBeenCalled()
    security.close()
  })

  test('maps a native system-pair rejection to the documented rejected result', async () => {
    const rejection = contractError('platform.failure', 'platform', 'android.security.pair', {
      domain: 'android',
      code: 'pairRejected',
      safeMessage: 'The Android user declined pairing.',
      metadata: {}
    })
    const security = new ReactNativeAndroidSecurityBackend(
      securityAdapter({ pair: jest.fn(async () => Promise.reject(rejection)) }),
      () => 20
    )

    await expect(
      security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'auto',
        protection: 'system-default',
        ceremony: 'system'
      })
    ).resolves.toEqual({ outcome: 'rejected', reason: 'The Android user declined pairing.' })
    security.close()
  })

  test('settles caller cancellation promptly while retaining peer ownership until native pairing settles', async () => {
    let resolveNative
    let firstPair = true
    const pair = jest.fn(() => {
      if (!firstPair) return Promise.resolve({ outcome: 'paired', state: securityState('bonded') })
      firstPair = false
      return new Promise(resolve => {
        resolveNative = resolve
      })
    })
    const cleanupPairing = jest.fn(async () => undefined)
    const security = new ReactNativeAndroidSecurityBackend(securityAdapter({ pair, cleanupPairing }), () => 20)
    const controller = new AbortController()
    const pending = security.pair(peerId, {
      signal: controller.signal,
      deadline: null,
      transport: 'auto',
      protection: 'system-default',
      ceremony: 'system'
    })

    controller.abort()
    await expect(pending).resolves.toEqual({ outcome: 'cancelled' })
    expect(cleanupPairing).toHaveBeenCalledWith(peerId)
    expect([...security.active]).toEqual([peerId])

    resolveNative({ outcome: 'paired', state: securityState('bonded') })
    await Promise.resolve()
    await expect(
      security.pair(peerId, {
        signal: null,
        deadline: null,
        transport: 'auto',
        protection: 'system-default',
        ceremony: 'system'
      })
    ).resolves.toMatchObject({ outcome: 'paired' })
    security.close()
  })
})

function securityState(bond) {
  return {
    bond,
    encryption: 'unsupported',
    authentication: 'unsupported',
    secureConnections: 'unsupported',
    pairingPossible: true,
    measuredAtMonotonicMs: 20,
    limitations: []
  }
}

function securityAdapter(overrides = {}) {
  return {
    securityState: jest.fn(async () => ({ ...securityState('not-bonded') })),
    pair: jest.fn(async () => ({ outcome: 'paired', state: securityState('bonded') })),
    cancelPairing: jest.fn(async () => undefined),
    cleanupPairing: jest.fn(async () => undefined),
    unpair: jest.fn(async () => 'unsupported'),
    onSecurityState: () => () => undefined,
    securityAvailable: true,
    securityCancellationAvailable: true,
    ...overrides
  }
}

async function openBoundary(control) {
  const boundary = new ReactNativeAndroidProtocolBoundary(control, 'android-security-owner')
  boundary.bindAttachment(attachment)
  await boundary.open()
  return boundary
}

class SecurityControl {
  constructor(securityAvailable, securityCancelPairingAvailable = securityAvailable) {
    this.securityAvailable = securityAvailable
    this.securityCancelPairingAvailable = securityCancelPairingAvailable
    this.closed = false
  }

  handshake() {
    return Promise.resolve({
      nativeProtocol: 2,
      abi: 2,
      backendContract: 1,
      capabilitySchema: 1,
      eventSchema: 1,
      traceFormat: 1,
      maximumControlRecordBytes: 65536,
      maximumBinaryPayloadBytes: 524288,
      securityAvailable: this.securityAvailable,
      securityCancelPairingAvailable: this.securityCancelPairingAvailable
    })
  }

  installExecutionRuntime() {
    return Promise.resolve()
  }

  closeAttachment() {
    this.closed = true
    return Promise.resolve()
  }
}

class SecurityRuntime {
  constructor(control) {
    this.control = control
    this.listener = null
    this.commands = []
    this.bondState = 'notBonded'
    this.resultPeerId = peerId
    this.eventAttachment = null
    this.deferPair = false
    this.pendingPair = null
  }

  setEventSink(listener) {
    this.listener = listener
    this.emit(
      protocolRecord('event', [
        field(1, 1),
        field(2, 'adapter-event-1'),
        field(3, 'adapterState'),
        field(4, this.activeAttachment()),
        field(5, 1),
        field(6, 20),
        field(15, protocolRecord('adapterStateSnapshot', [field(1, 'available'), field(2, 'granted'), field(3, 'on')]))
      ])
    )
  }

  setFatalSink() {}

  submit(bytes) {
    const command = decodeNativeProtocolRecord(bytes)
    const kind = requiredString(command, 3, 'test.command.kind')
    this.commands.push(kind)
    if (kind === 'securityState') {
      this.emitResult(command, 'securityState', this.resultPeerId, this.bondState)
      return
    }
    if (kind === 'securityPair') {
      if (this.deferPair) {
        this.deferPair = false
        this.pendingPair = command
        return
      }
      this.bondState = 'bonded'
      this.emitResult(command, 'securityPair', peerId, this.bondState)
      return
    }
    if (kind === 'securityCancelPairing') {
      if (this.pendingPair !== null) {
        const pending = this.pendingPair
        this.pendingPair = null
        this.emitFailure(pending, 'cancelled', 'Android security pairing cleanup released ownership')
      }
      this.emitResult(command, 'accepted')
      return
    }
    if (kind === 'destroy') {
      this.emitResult(command, 'destroyed')
      return
    }
    throw new Error(`Unexpected test command ${kind}`)
  }

  emitSecurityState(nativePeerId, bondState) {
    this.emit(
      protocolRecord('event', [
        field(1, 1),
        field(2, 'security-event-1'),
        field(3, 'securityStateChanged'),
        field(4, this.activeAttachment()),
        field(5, 1),
        field(6, 20),
        field(16, nativePeerId),
        field(17, bondState)
      ])
    )
  }

  emitResult(command, kind, nativePeerId, bondState) {
    const fields = [
      field(1, 1),
      field(2, kind),
      field(
        3,
        protocolRecord('terminal', [field(1, requiredRecord(command, 2, 'test.correlation')), field(2, 'succeeded')])
      )
    ]
    if (nativePeerId !== undefined) fields.push(field(16, nativePeerId), field(17, bondState))
    this.emit(protocolRecord('result', fields))
  }

  emitFailure(command, code, safeMessage) {
    this.emit(
      protocolRecord('result', [
        field(1, 1),
        field(2, 'cancelled'),
        field(
          3,
          protocolRecord('terminal', [
            field(1, requiredRecord(command, 2, 'test.correlation')),
            field(2, 'failed'),
            field(3, code)
          ])
        ),
        field(
          10,
          protocolRecord('error', [
            field(1, code),
            field(2, 'android'),
            field(3, requiredString(command, 3, 'test.command.kind')),
            field(4, 'notRetryable'),
            field(7, safeMessage)
          ])
        )
      ])
    )
  }

  emit(record) {
    if (this.listener === null) throw new Error('The test event sink is not installed')
    this.listener(encodeNativeProtocolRecord(record))
  }

  activeAttachment() {
    if (this.eventAttachment !== null) return this.eventAttachment
    return protocolRecord('attachment', [
      field(1, attachment.attachmentId),
      field(2, attachment.backendInstanceId),
      field(3, attachment.backendGeneration),
      field(4, attachment.adapterId),
      field(5, attachment.adapterGeneration)
    ])
  }

  retain() {
    throw contractError('capability.unsupported', 'test', 'security-runtime.retain')
  }

  copy() {
    throw contractError('capability.unsupported', 'test', 'security-runtime.copy')
  }

  release() {
    return true
  }

  retainedByteCount() {
    return 0
  }

  retainedPayloadCount() {
    return 0
  }
}
