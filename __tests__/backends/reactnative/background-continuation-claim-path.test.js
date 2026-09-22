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
  consumerCount: 1,
  selectors: [{ serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', serviceOccurrence: 1, characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb', characteristicOccurrence: 1 }],
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
  disposed: true,
  afterCutoffLoss: { items: 0, bytes: 0 }
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

function managerHost(native, declaration = NATIVE_DECLARATION) {
  const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
  return createReactNativeManagerHost({
    platform: 'android',
    now: () => 0,
    clientId: 'client-1',
    managerId: 'manager-1',
    hostSessionScope: 'ubm-host:test',
    androidApiLevel: 34,
    rustCore: binding,
    ...(declaration === null ? {} : { background: { continuation: declaration } })
  })
}

describe('continuation claim path', () => {
  it('drains the wake backlog with its loss accounting', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const acknowledgements = []
    native.declareBackgroundContinuation = async () => {}
    native.prepareContinuationClaim = async () => JSON.stringify({ ...JSON.parse(CLAIM_JSON), claimToken: 'claim-1' })
    native.acknowledgeContinuationClaim = async token => {
      acknowledgements.push(token)
      return JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)
    const backlog = await host.services.claimContinuationBacklog()
    expect(backlog.values).toHaveLength(1)
    expect([...backlog.values[0].value]).toEqual([0, 72])
    expect(backlog.streamEnds).toEqual([
      { consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }
    ])
    expect(backlog.disposed).toBe(true)
    expect(acknowledgements).toEqual(['claim-1'])
    const status = await host.services.continuationStatus()
    expect(status.strategy).toBe('native')
    expect(status.lastWake.event).toBe('continuation.completed')
    await host.manager.destroy()
  })

  it('claims a manifest-declared backlog from the native owner truth when JS omits the declaration', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.prepareContinuationClaim = async () => JSON.stringify({ ...JSON.parse(CLAIM_JSON), claimToken: 'claim-1' })
    native.acknowledgeContinuationClaim = async () => JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    native.continuationStatus = async () => JSON.stringify({ ...JSON.parse(STATUS_JSON), resubscribe: 0 })
    const host = await managerHost(native, null)

    const backlog = await host.services.claimContinuationBacklog()

    expect(backlog.values).toHaveLength(1)
    expect(backlog.values[0].consumer).toBe('ubm-continuation-0')
    await host.manager.destroy()
  })

  it('does not acknowledge an invalid prepared batch and replays the exact token and batches', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const prepared = JSON.stringify({
      ...JSON.parse(CLAIM_JSON),
      claimToken: 'claim-replay',
      batches: [backlogBatch([{ t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: 7, delivery: 'notification' }])]
    })
    const acknowledgements = []
    native.prepareContinuationClaim = async () => prepared
    native.acknowledgeContinuationClaim = async token => {
      acknowledgements.push(token)
      return JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    await expect(host.services.claimContinuationBacklog()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    await expect(host.services.claimContinuationBacklog()).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    expect(acknowledgements).toEqual([])
    await host.manager.destroy()
  })

  it('returns a decoded backlog with disposal uncertainty when acknowledgement is malformed', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    native.prepareContinuationClaim = async () => JSON.stringify({ ...JSON.parse(CLAIM_JSON), claimToken: 'claim-uncertain' })
    native.acknowledgeContinuationClaim = async () => JSON.stringify({ disposed: 'unknown' })
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const backlog = await host.services.claimContinuationBacklog()

    expect(backlog.values).toHaveLength(1)
    expect(backlog.disposed).toBe(false)
    expect(backlog.disposeFailure).toMatch(/acknowledgement/i)
    await host.manager.destroy()
  })

  it('returns a decoded backlog with disposal uncertainty when acknowledgement transport rejects', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    native.prepareContinuationClaim = async () => JSON.stringify({ ...JSON.parse(CLAIM_JSON), claimToken: 'claim-transport' })
    native.acknowledgeContinuationClaim = async () => Promise.reject(new Error('bridge disconnected'))
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const backlog = await host.services.claimContinuationBacklog()

    expect(backlog.values).toHaveLength(1)
    expect(backlog.disposed).toBe(false)
    expect(backlog.disposeFailure).toMatch(/acknowledgement/i)
    await host.manager.destroy()
  })

  it('uses an empty acknowledged receipt on the follow-up claim without redelivering decoded batches', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    let prepares = 0
    let acknowledgements = 0
    native.prepareContinuationClaim = async () => {
      prepares += 1
      if (prepares === 1) return JSON.stringify({ ...JSON.parse(CLAIM_JSON), claimToken: 'claim-receipt' })
      return JSON.stringify({
        consumerCount: 1,
        selectors: JSON.parse(CLAIM_JSON).selectors,
        batches: [],
        disposed: false,
        afterCutoffLoss: { items: 0, bytes: 0 },
        disposeFailure: null,
        claimToken: 'claim-receipt'
      })
    }
    native.acknowledgeContinuationClaim = async () => {
      acknowledgements += 1
      if (acknowledgements === 1) return JSON.stringify({ disposed: 'unknown' })
      return JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const first = await host.services.claimContinuationBacklog()
    const receipt = await host.services.claimContinuationBacklog()

    expect(first.values).toHaveLength(1)
    expect(first.disposed).toBe(false)
    expect(receipt.values).toEqual([])
    expect(receipt.disposed).toBe(true)
    await host.manager.destroy()
  })

  it('acknowledges an empty prepared handoff so a zero-selector wake is not left sealed', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const acknowledgements = []
    native.prepareContinuationClaim = async () => JSON.stringify({
      consumerCount: 0,
      selectors: [],
      batches: [],
      disposed: false,
      afterCutoffLoss: { items: 0, bytes: 0 },
      disposeFailure: null,
      claimToken: 'empty-wake'
    })
    native.acknowledgeContinuationClaim = async token => {
      acknowledgements.push(token)
      return JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const backlog = await host.services.claimContinuationBacklog()

    expect(backlog.disposed).toBe(true)
    expect(acknowledgements).toEqual(['empty-wake'])
    await host.manager.destroy()
  })

  it('accepts the tokenless empty no-wake answer without an acknowledgement', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const acknowledgements = []
    native.prepareContinuationClaim = async () => JSON.stringify({
      consumerCount: 0,
      selectors: [],
      batches: [],
      disposed: false,
      afterCutoffLoss: { items: 0, bytes: 0 },
      disposeFailure: null
    })
    native.acknowledgeContinuationClaim = async token => {
      acknowledgements.push(token)
      return JSON.stringify({ disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const backlog = await host.services.claimContinuationBacklog()

    expect(backlog.values).toEqual([])
    expect(backlog.disposed).toBe(false)
    expect(acknowledgements).toEqual([])
    await host.manager.destroy()
  })

  it('acknowledges an incomplete prefix before claiming its tail without redelivery', async () => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    native.declareBackgroundContinuation = async () => {}
    const prepared = [
      JSON.stringify({
        ...JSON.parse(CLAIM_JSON),
        claimToken: 'prefix',
        batches: [backlogBatch([{ t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: HR_VALUE_B64, delivery: 'notification' }], { more: true })],
        disposed: false,
        disposeFailure: 'continuation drain batch malformed: more is missing or not boolean'
      }),
      JSON.stringify({
        ...JSON.parse(CLAIM_JSON),
        claimToken: 'tail',
        batches: [backlogBatch([{ t: 'value', ordinal: 3, consumer: 'ubm-continuation-0', valueB64: 'AQ==', delivery: 'notification' }])],
        disposed: false,
        disposeFailure: null
      })
    ]
    const acknowledgements = []
    native.prepareContinuationClaim = async () => prepared.shift()
    native.acknowledgeContinuationClaim = async token => {
      acknowledgements.push(token)
      return JSON.stringify(token === 'prefix'
        ? { disposed: false, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: 'continuation drain batch malformed: more is missing or not boolean' }
        : { disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })
    }
    native.continuationStatus = async () => STATUS_JSON
    const host = await managerHost(native)

    const prefix = await host.services.claimContinuationBacklog()
    const tail = await host.services.claimContinuationBacklog()

    expect(prefix.values.map(value => value.consumer)).toEqual(['ubm-continuation-0'])
    expect(prefix.disposed).toBe(false)
    expect(prefix.disposeFailure).toMatch(/malformed/)
    expect(tail.values.map(value => value.consumer)).toEqual(['ubm-continuation-0'])
    expect(tail.disposed).toBe(true)
    expect(acknowledgements).toEqual(['prefix', 'tail'])
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
