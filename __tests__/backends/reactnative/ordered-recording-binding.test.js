// __tests__/backends/reactnative/ordered-recording-binding.test.js
jest.mock('react-native', () => ({ Platform: { OS: 'android', Version: 34 }, TurboModuleRegistry: { get: () => null } }))
const { DeterministicRustCoreNative } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')

test('ordered recording controls preserve exact authenticated session and declaration', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  native.orderedRecordingVersion = jest.fn(async () => 1)
  native.orderedRecordingReserve = jest.fn(async () => '{"ok":true,"value":null}')
  native.orderedRecordingStartStatus = jest.fn(async () => '{"ok":true,"value":null}')
  native.orderedRecordingRetire = jest.fn(async () => '{"ok":true,"value":null}')
  native.orderedRecordingStart = jest.fn(async () => '{"ok":true,"value":{}}')
  native.orderedRecordingContext = jest.fn(async () => '{"ok":true,"value":{}}')
  native.orderedRecordingStop = jest.fn(async () => '{"ok":true,"value":{}}')
  const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
  const session = await binding.openSession('owner')
  expect(await session.orderedRecordingVersion()).toBe(1)
  await session.orderedRecordingStart('handle', 'r', '1000', '10', '{"kind":"ordered-recording"}', 5000)
  await session.orderedRecordingContext('r')
  await session.orderedRecordingStop('r')
  expect(native.orderedRecordingStart).toHaveBeenCalledWith(session.sessionId, 'handle', 'r', '1000', '10', '{"kind":"ordered-recording"}', 5000)
  expect(native.orderedRecordingContext).toHaveBeenCalledWith(session.sessionId, 'r')
  expect(native.orderedRecordingStop).toHaveBeenCalledWith(session.sessionId, 'r')
  await session.close()
  await expect(session.orderedRecordingStop('r')).rejects.toThrow()
})

test('partial native recording ABI is not exposed', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  native.orderedRecordingVersion = async () => 1
  const session = await createReactNativeRustCoreBinding({ platform: 'android', native }).openSession('owner')
  expect(session.orderedRecordingStart).toBeUndefined()
  expect(session.orderedRecordingStop).toBeUndefined()
  await session.close()
})

test('offline ordered context verifies binary identity and does not open a session', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  native.orderedRecordingOfflineContext = jest.fn(async () => '{"ok":true,"value":{}}')
  const binding = createReactNativeRustCoreBinding({ platform: 'android', native })
  await binding.orderedRecordingOfflineContext('r')
  expect(native.orderedRecordingOfflineContext).toHaveBeenCalledWith('r')
  expect(native.calls.some(call => call[0] === 'openSession')).toBe(false)
  native.identity = { ...native.identity, sourceDigest: '0'.repeat(64) }
  await expect(binding.orderedRecordingOfflineContext('r')).rejects.toThrow()
  expect(native.orderedRecordingOfflineContext).toHaveBeenCalledTimes(1)
})
