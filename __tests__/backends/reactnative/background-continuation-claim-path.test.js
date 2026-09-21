// __tests__/backends/reactnative/background-continuation-claim-path.test.js
//
// BGS4: when the app opens after a wake, the backlog drains through the
// claim with its loss accounting. The native claim returns verbatim drain
// batches; the provider aggregates them through the drain codec. A native
// module without the claim answers `capability.unsupported` — never an
// invented empty backlog.

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 34 },
  TurboModuleRegistry: { get: () => null },
  NativeModules: {}
}))

const { createReactNativeManagerHost } = require('../../../src/react-native-manager')
const {
  createReactNativeRustCoreBinding
} = require('../../../src/backends/reactnative/react-native-rust-core-binding')
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')

const HR_VALUE_B64 = 'AEg='
const NATIVE_DECLARATION = Object.freeze({
  onAppearance: 'native',
  resubscribe: Object.freeze([
    { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb' }
  ])
})

function backlogBatch(records, { more = false, controlLost = 0 } = {}) {
  return JSON.stringify({ more, records, controlLost })
}

const CLAIM_JSON = JSON.stringify({
  batches: [
    backlogBatch(
      [
        { t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: HR_VALUE_B64, delivery: 'notification' },
        {
          t: 'stream-end',
          ordinal: 2,
          consumer: 'ubm-continuation-0',
          reason: 'overflow',
          droppedItems: 5,
          droppedBytes: 100
        }
      ],
      { more: false }
    )
  ],
  disposed: true
})

const STATUS_JSON = JSON.stringify({
  strategy: 'native',
  peerId: null,
  resubscribe: 1,
  malformedDeclarations: 0,
  lastWake: {
    observedAtMs: 12345,
    event: 'continuation.completed',
    strategy: 'native',
    peerAddress: 'A0:9E:1A:E9:B9:3D',
    code: null,
    reason: null
  }
})

function managerHost(native) {
  const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
  return createReactNativeManagerHost({
    platform: 'android',
    now: () => 0,
    clientId: 'client-1',
    managerId: 'manager-1',
    hostSessionScope: 'ubm-host:test',
    androidApiLevel: 34,
    rustCore: binding,
    background: { continuation: NATIVE_DECLARATION }
  })
}

describe('continuation claim path', () => {
  it('drains the wake backlog with its loss accounting', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    native.claimContinuation = async () => CLAIM_JSON
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)
    const backlog = await host.services.claimContinuationBacklog()
    expect(backlog.values).toHaveLength(1)
    expect([...backlog.values[0].value]).toEqual([0, 72])
    expect(backlog.streamEnds).toEqual([
      { consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }
    ])
    expect(backlog.disposed).toBe(true)
    const status = await host.services.continuationStatus()
    expect(status.strategy).toBe('native')
    expect(status.lastWake.event).toBe('continuation.completed')
    await host.manager.destroy()
  })

  it('a native module without the claim answers unsupported, never an invented empty backlog', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const host = await managerHost(native)
    await expect(host.services.claimContinuationBacklog()).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    await host.manager.destroy()
  })
})
