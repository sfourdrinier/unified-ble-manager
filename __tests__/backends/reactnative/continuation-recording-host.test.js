const { createReactNativeContinuationRecordings } = require('../../../src/react-native-continuation-recording')

test('mobile recording access uses identity-checked binding without a manager or radio session', async () => {
  class Binding {
    id = 'h10'
    openSession = jest.fn(() => {
      throw new Error('radio permission denied')
    })
    async continuationRecordingPrepare(id) {
      expect(id).toBe(this.id)
      return JSON.stringify({ ok: true, value: { token: null, records: [], bytes: 0, more: false } })
    }
  }
  const binding = new Binding()
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  expect(await recordings.prepare('h10', { maxItems: 10, maxBytes: 4096 })).toEqual({
    token: null,
    records: [],
    bytes: 0,
    more: false
  })
  expect(binding.openSession).not.toHaveBeenCalled()
})

test('an older or injected binding without recording controls refuses explicitly', async () => {
  const recordings = createReactNativeContinuationRecordings({ rustCore: {} })
  await expect(recordings.status('h10')).rejects.toMatchObject({ code: 'capability.unsupported' })
  await expect(recordings.stop('h10')).rejects.toMatchObject({ code: 'capability.unsupported' })
})

test('unsupported default host throws the public typed error without attempting native access', () => {
  expect(() => createReactNativeContinuationRecordings()).toThrow(BleError)
})
jest.mock('react-native', () => ({ Platform: { OS: 'web' }, TurboModuleRegistry: { get: jest.fn() } }))
const { BleError } = require('../../../src/public/errors')


test('recovery hint subscribes without a native session and detaches independently', () => {
  const listeners = new Set()
  const binding = {
    openSession: jest.fn(),
    onRecoveryHint: listener => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    }
  }
  const recordings = createReactNativeContinuationRecordings({ rustCore: binding })
  const listener = jest.fn()
  const remove = recordings.onRecoveryHint(listener)
  for (const callback of listeners) callback()
  expect(listener).toHaveBeenCalledTimes(1)
  remove()
  for (const callback of listeners) callback()
  expect(listener).toHaveBeenCalledTimes(1)
  expect(binding.openSession).not.toHaveBeenCalled()
})

test('missing recovery hint capability fails explicitly', () => {
  const recordings = createReactNativeContinuationRecordings({ rustCore: {} })
  expect(() => recordings.onRecoveryHint(() => {})).toThrow(BleError)
})
