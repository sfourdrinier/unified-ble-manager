const { negotiateReactNativeRecordingAuthority, supportsReactNativeRecordingAuthority } = require('../../../src/backends/reactnative/react-native-recording-authority')

test('legacy reclaim versions do not advertise an authority adapter', () => {
  expect(supportsReactNativeRecordingAuthority(0)).toBe(false)
  expect(supportsReactNativeRecordingAuthority(2)).toBe(false)
})

test('authority negotiation accepts only a matching implemented adapter', () => {
  const adapter = { version: 'test-authority/1', commit() {}, status() {}, fence() {}, snapshot() {} }
  expect(negotiateReactNativeRecordingAuthority('test-authority/1', [adapter])).toBe(adapter)
  expect(negotiateReactNativeRecordingAuthority('test-authority/2', [adapter])).toBeNull()
  expect(negotiateReactNativeRecordingAuthority('test-authority/1', [])).toBeNull()
  expect(negotiateReactNativeRecordingAuthority(null, [adapter])).toBeNull()
})

test('ambiguous adapter versions are rejected', () => {
  const adapter = { version: 'test-authority/1', commit() {}, status() {}, fence() {}, snapshot() {} }
  expect(() => negotiateReactNativeRecordingAuthority('test-authority/1', [adapter, adapter])).toThrow('protocol.incompatible')
})

test('a matching version without every authority operation is rejected', () => {
  expect(() => negotiateReactNativeRecordingAuthority('test-authority/1', [{ version: 'test-authority/1' }])).toThrow('protocol.incompatible')
})
