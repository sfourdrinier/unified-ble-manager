// __tests__/backends/reactnative/background-continuation.test.js
//
// BGS4: `background.continuation` — the declared standing order the OS wake
// executes. Tests first: the contract module does not exist yet.

const {
  normalizeBackgroundContinuation,
  DEFAULT_BACKGROUND_CONTINUATION,
  CONTINUATION_STRATEGIES,
  CONTINUATION_OUTCOME_EVENTS,
  BACKGROUND_CONTINUATION_FEATURE_IDS
} = require('../../../src/backend-contract/background-continuation')
const { BackendContractError } = require('../../../src/backend-contract/errors')

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'

describe('background.continuation declaration', () => {
  it('defaults to record-only (today behaviour) when absent', () => {
    expect(normalizeBackgroundContinuation(undefined)).toEqual(
      Object.freeze({ onAppearance: 'record-only', resubscribe: Object.freeze([]) })
    )
    expect(DEFAULT_BACKGROUND_CONTINUATION.onAppearance).toBe('record-only')
    expect(CONTINUATION_STRATEGIES).toEqual(['record-only', 'native', 'headless-task', 'foreground-service'])
  })

  it('accepts a native standing order with declared resubscribe characteristics', () => {
    const declared = normalizeBackgroundContinuation({
      onAppearance: 'native',
      peerId: 'a0:9e:1a:e9:b9:3d',
      resubscribe: [{ serviceUuid: HR_SERVICE, characteristicUuid: HR_MEASUREMENT }]
    })
    expect(declared.onAppearance).toBe('native')
    expect(declared.peerId).toBe('A0:9E:1A:E9:B9:3D')
    expect(declared.resubscribe).toHaveLength(1)
    expect(declared.resubscribe[0]).toMatchObject({
      serviceUuid: HR_SERVICE,
      characteristicUuid: HR_MEASUREMENT,
      serviceOccurrence: 1,
      characteristicOccurrence: 1
    })
    expect(Object.isFrozen(declared)).toBe(true)
  })

  it('rejects an unknown strategy instead of substituting a quieter path', () => {
    let error = null
    try {
      normalizeBackgroundContinuation({ onAppearance: 'auto-magic' })
    } catch (e) {
      error = e
    }
    expect(error).toBeInstanceOf(BackendContractError)
    expect(error.normalized.code).toBe('argument.invalid')
  })

  it('rejects unknown keys fail-closed', () => {
    expect(() =>
      normalizeBackgroundContinuation({ onAppearance: 'record-only', reconnectIntervalMs: 1000 })
    ).toThrow()
  })

  it('requires resubscribe entries to name a service and characteristic', () => {
    expect(() =>
      normalizeBackgroundContinuation({ onAppearance: 'native', resubscribe: [{ serviceUuid: HR_SERVICE }] })
    ).toThrow()
    expect(() =>
      normalizeBackgroundContinuation({ onAppearance: 'native', resubscribe: 'hr' })
    ).toThrow()
  })

  it('requires headless-task to name its task, never falling back silently', () => {
    expect(() => normalizeBackgroundContinuation({ onAppearance: 'headless-task' })).toThrow()
    const declared = normalizeBackgroundContinuation({
      onAppearance: 'headless-task',
      headlessTaskName: 'BleWakeTask'
    })
    expect(declared.headlessTaskName).toBe('BleWakeTask')
  })

  it('accepts the foreground-service shape so rc.1 needs no breaking change', () => {
    const declared = normalizeBackgroundContinuation({
      onAppearance: 'foreground-service',
      foregroundService: {
        notification: { channelId: 'ble', channelName: 'BLE', title: 'BLE streaming' }
      }
    })
    expect(declared.foregroundService.notification.title).toBe('BLE streaming')
    expect(() => normalizeBackgroundContinuation({ onAppearance: 'foreground-service' })).toThrow()
  })
})

describe('background.continuation outcome vocabulary (one vocabulary, both hosts)', () => {
  it('uses the same completed/failed event names on every platform', () => {
    expect(CONTINUATION_OUTCOME_EVENTS).toEqual(['continuation.completed', 'continuation.failed'])
  })
})

describe('background.continuation capability identifiers', () => {
  it('names one capability per strategy', () => {
    expect(BACKGROUND_CONTINUATION_FEATURE_IDS).toEqual(
      Object.freeze({
        wakeOnAppearance: 'background:wake-on-appearance',
        nativeResubscribe: 'background:native-resubscribe',
        headlessTask: 'background:headless-task',
        wakeNotification: 'background:wake-notification'
      })
    )
  })
})
