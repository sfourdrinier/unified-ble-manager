// __tests__/backends/reactnative/rust-core-binding.test.js
//
// R01/D3(a): the production React Native binding producer over the
// `UnifiedBleRustCore` TurboModule. Missing module fails loud
// (`capability.unsupported`, never a crash); sessions open/invoke/close
// through the native surface; failure records keep their exact wire
// identity; successful JSON values decode to structured results.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: () => mockNativeModule,
    getEnforcing: () => {
      throw new Error('spec default must never load in the producer path')
    }
  },
  NativeModules: {}
}))

const {
  admitReactNativeRustCoreSession,
  RUST_CORE_CONTRACT_REVISION
} = require('../../../src/backends/reactnative/react-native-rust-core')
const { createReactNativeRustCoreBinding } = require('../../../src/backends/reactnative/react-native-rust-core-binding')

function record(overrides = {}) {
  return {
    ok: true,
    value: '',
    code: '',
    domain: '',
    operation: 'test.op',
    ...overrides
  }
}

function presentNative(overrides = {}) {
  const calls = []
  mockNativeModule = {
    openSession: async owner => {
      calls.push(['openSession', owner])
      return { sessionId: 'session-1' }
    },
    invoke: async (sessionId, op, argsJson) => {
      calls.push(['invoke', sessionId, op, argsJson])
      return record()
    },
    close: async sessionId => {
      calls.push(['close', sessionId])
    },
    contractRevision: async () => RUST_CORE_CONTRACT_REVISION,
    ...overrides
  }
  return calls
}

beforeEach(() => {
  mockNativeModule = null
})

async function captureFailure(attempt) {
  try {
    await attempt()
  } catch (failure) {
    return failure
  }
  return null
}

describe('production Rust core binding (D3a)', () => {
  test('missing native module fails loud, never crashes', async () => {
    const error = await captureFailure(() => createReactNativeRustCoreBinding())
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('capability.unsupported')
    expect(error.normalized.operation).toBe('react-native-manager.rust-core-missing')
  })

  test('openSession admits the pinned revision through the seam', async () => {
    presentNative()
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    await expect(admitReactNativeRustCoreSession(session)).resolves.toBe(session)
    expect(session.contractRevision()).toBe(RUST_CORE_CONTRACT_REVISION)
  })

  test('revision skew surfaces via admission', async () => {
    presentNative({ contractRevision: async () => 'C-UBM.9.9.9-DRAFT' })
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    const error = await captureFailure(() => admitReactNativeRustCoreSession(session))
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('protocol.incompatible')
  })

  test('invoke decodes JSON values and crosses args verbatim', async () => {
    const calls = presentNative({
      invoke: async (sessionId, op, argsJson) => {
        calls.push(['invoke', sessionId, op, argsJson])
        return record({ value: '{"operationId":"apple-scan-op-1"}', operation: 'scan.start' })
      }
    })
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    await expect(session.invoke('scan.start', { owner: 'o' })).resolves.toEqual({
      operationId: 'apple-scan-op-1'
    })
    expect(calls).toContainEqual(['invoke', 'session-1', 'scan.start', '{"owner":"o"}'])
  })

  test('empty values decode to null (quiet take)', async () => {
    presentNative({ invoke: async () => record({ value: '', operation: 'scan.take' }) })
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    await expect(session.invoke('scan.take', {})).resolves.toBeNull()
  })

  test('failure records keep their exact wire identity', async () => {
    presentNative({
      invoke: async () =>
        record({ ok: false, value: '', code: 'lifecycle.destroyed', domain: 'core', operation: 'central-status' })
    })
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    const error = await captureFailure(() => session.invoke('central.status', {}))
    expect(error).not.toBeNull()
    expect(error.normalized.code).toBe('lifecycle.destroyed')
    expect(error.normalized.domain).toBe('core')
    expect(error.normalized.operation).toBe('central-status')
  })

  test('close reaches the native session', async () => {
    const calls = presentNative()
    const binding = createReactNativeRustCoreBinding()
    const session = await binding.openSession('owner-a')
    await session.close()
    expect(calls).toContainEqual(['close', 'session-1'])
  })
})
