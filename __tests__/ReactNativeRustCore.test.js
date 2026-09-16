// __tests__/ReactNativeRustCore.test.js
//
// F01 React Native shared-core seam: selection, binding resolution, and
// contract admission between the RN factory layer and the native Rust core.
// The seam never constructs (or falls back to) the TypeScript manager: a
// missing binding or a foreign revision fails loudly, so the F01
// acceptance proof can never silently exercise the old TS runtime.

'use strict'

const {
  RUST_CORE_CONTRACT_REVISION,
  admitReactNativeRustCoreSession,
  dispatchReactNativeRustCoreOp,
  openAdmittedRustCoreSession,
  resolveReactNativeRustCoreBinding
} = require('../src/backends/reactnative/react-native-rust-core')
const { CONTRACT_REVISION } = require('../contracts/src/version')

function fakeSession(revision = CONTRACT_REVISION) {
  const calls = []
  return {
    calls,
    session: {
      contractRevision: () => revision,
      invoke: async (op, args) => {
        calls.push([op, args])
        return { op, echoed: args }
      },
      close: async () => {
        calls.push(['close', undefined])
      }
    }
  }
}

function fakeBinding(session) {
  return {
    openSession: async owner => {
      if (typeof owner !== 'string' || owner.length === 0) throw new Error('owner must not be empty')
      return session
    }
  }
}

describe('React Native Rust core seam (F01)', () => {
  test('pins the frozen contract revision', () => {
    expect(RUST_CORE_CONTRACT_REVISION).toBe(CONTRACT_REVISION)
  })

  test('missing binding fails loudly, never a TS fallback', () => {
    for (const candidate of [undefined, null, {}, { openSession: 'yes' }, 42]) {
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

  test('foreign contract revision fails closed', async () => {
    const { session } = fakeSession('C-UBM.9.9.9-DRAFT')
    const error = await admitReactNativeRustCoreSession(session).then(
      () => null,
      thrown => thrown
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('protocol.incompatible')
    expect(error.normalized.operation).toBe('react-native-manager.rust-core-revision')
  })

  test('admitted session dispatches ops without TS interpretation', async () => {
    const { calls, session } = fakeSession()
    const binding = resolveReactNativeRustCoreBinding(fakeBinding(session))
    const admitted = await openAdmittedRustCoreSession(binding, 'owner-a')
    expect(admitted).toBe(session)
    for (const [op, args] of [
      ['scan.start', { serviceUuids: [] }],
      ['connection.connect', { peerId: 'peer-1' }],
      ['gatt.subscribe', { peerId: 'peer-1' }],
      ['connection.disconnect', { peerId: 'peer-1' }]
    ]) {
      const result = await dispatchReactNativeRustCoreOp(admitted, op, args)
      expect(result).toEqual({ op, echoed: args })
    }
    await admitted.close()
    expect(calls.map(([op]) => op)).toEqual([
      'scan.start',
      'connection.connect',
      'gatt.subscribe',
      'connection.disconnect',
      'close'
    ])
  })

  test('empty op names fail closed', async () => {
    const { session } = fakeSession()
    const error = await dispatchReactNativeRustCoreOp(session, '', {}).then(
      () => null,
      thrown => thrown
    )
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('argument.invalid')
  })
})
