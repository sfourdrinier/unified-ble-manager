// __tests__/ReactNativeRustCore.test.js
//
// F01 React Native shared-core seam: binding resolution and the pinned
// contract revision. A missing or malformed binding fails loudly — there is
// no TypeScript fallback. Admission, identity and dispatch are proven through
// the production binding in __tests__/backends/reactnative/rust-core-binding.test.js.

'use strict'

const {
  RUST_CORE_CONTRACT_REVISION,
  resolveReactNativeRustCoreBinding
} = require('../src/backends/reactnative/react-native-rust-core')
const { CONTRACT_REVISION } = require('../contracts/src/version')

describe('React Native Rust core seam (F01)', () => {
  test('pins the frozen contract revision', () => {
    expect(RUST_CORE_CONTRACT_REVISION).toBe(CONTRACT_REVISION)
  })

  test('a missing or malformed binding fails loudly, never a TS fallback', () => {
    for (const candidate of [
      undefined,
      null,
      {},
      { openSession: 'yes' },
      42,
      { openSession: async () => null },
      { openSession: async () => null, randomBytes: async () => null }
    ]) {
      let error = null
      try {
        resolveReactNativeRustCoreBinding(candidate)
      } catch (thrown) {
        error = thrown
      }
      expect(error).not.toBeNull()
      expect(error.normalized.code).toBe('capability.unsupported')
      expect(error.normalized.operation).toBe('react-native-manager.rust-core-missing')
    }
  })

  test('a complete binding resolves as itself', () => {
    const binding = {
      openSession: async () => null,
      randomBytes: async () => new Uint8Array(0),
      restorationIdentity: async () => null,
      configuredRestorationIdentity: async () => null
    }
    expect(resolveReactNativeRustCoreBinding(binding)).toBe(binding)
  })
})
