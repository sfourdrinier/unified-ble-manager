'use strict'
const { bindDesktopCore } = require('../../../src/desktop-core-addon')
const { EXPECTED_NATIVE_BUILD_IDENTITY: expected } = require('../../../src/generated/native-build-identity')
const host = { platform: 'corebluetooth', operationPrefix: 'direct-gatt' }
const identity = {
  schema: 'ubm-native-build-identity/1',
  binding: 'napi',
  contractRevision: expected.contractRevision,
  sourceDigest: expected.bindings.napi.sourceDigest,
  bindingSchema: expected.bindings.napi.bindingSchema,
  target: 'aarch64-apple-darwin',
  profile: 'release',
  features: [],
  rustc: 'rustc 1.98.1'
}
function fixture(overrides = {}) {
  const calls = []
  class ContinuationRecordingStore {
    constructor() {
      this.calls = calls
    }
    async configureDirectory(path) {
      this.calls.push(['configure', path])
      return JSON.stringify({ ok: true, value: { state: 'configured', encrypted: false } })
    }
    async status(id) {
      this.calls.push(['status', id])
      return JSON.stringify({ ok: true, value: { recordingId: id } })
    }
    async prepare() {
      return '{}'
    }
    async acknowledge() {
      return '{}'
    }
    async stop() {
      return '{}'
    }
    async clear() {
      return '{}'
    }
  }
  const radio = () => {
    throw new Error('radio must never open')
  }
  const module = {
    nativeBuildIdentity: () => JSON.stringify(identity),
    UbmCentral: {
      open: radio,
      openSynthetic: radio,
      listAdapters: radio,
      capabilityStates: () => [],
      vendoredBtleplugPatches: () => []
    },
    ContinuationRecordingStore,
    ...overrides
  }
  return { calls, loaded: { module, path: '/offline/addon.node', mode: 'source', sidecar: null } }
}
test('offline store preserves receivers and never initializes or enumerates radio', async () => {
  const { loaded, calls } = fixture()
  const store = await bindDesktopCore(host, loaded).openRecordingStore('/private/app/recordings')
  await store.status('sample')
  expect(calls).toEqual([
    ['configure', '/private/app/recordings'],
    ['status', 'sample']
  ])
})
test.each(['configureDirectory', 'status', 'prepare', 'acknowledge', 'stop', 'clear'])(
  'offline %s normalizes native promise rejection',
  async method => {
    const Base = fixture().loaded.module.ContinuationRecordingStore
    class RejectingStore extends Base {}
    RejectingStore.prototype[method] = async () => {
      throw new Error('native worker refused')
    }
    const { loaded } = fixture({ ContinuationRecordingStore: RejectingStore })
    const binding = bindDesktopCore(host, loaded)
    const attempt =
      method === 'configureDirectory'
        ? binding.openRecordingStore('/private/app')
        : (await binding.openRecordingStore('/private/app'))[method]('sample', 1, 1024)
    await expect(attempt).rejects.toMatchObject({
      normalized: {
        code: 'platform.transport',
        operation: 'direct-gatt.recording-store',
        platform: { code: 'binding-rejection', safeMessage: 'native worker refused' }
      }
    })
  }
)
test('offline invocation preserves contract error identity and validates successful result shape', async () => {
  const { contractError } = require('../../../src/backend-contract/errors')
  const failure = contractError('permission.denied', 'platform', 'test.storage')
  const Base = fixture().loaded.module.ContinuationRecordingStore
  class InvalidStore extends Base {
    async status() {
      throw failure
    }
    async clear() {
      return 42
    }
  }
  const { loaded } = fixture({ ContinuationRecordingStore: InvalidStore })
  const store = await bindDesktopCore(host, loaded).openRecordingStore('/private/app')
  await expect(store.status('sample')).rejects.toBe(failure)
  await expect(store.clear('sample')).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})
test('identity mismatch fails before store construction', () => {
  const constructor = jest.fn()
  const { loaded } = fixture({
    ContinuationRecordingStore: constructor,
    nativeBuildIdentity: () => JSON.stringify({ ...identity, sourceDigest: 'wrong' })
  })
  expect(() => bindDesktopCore(host, loaded)).toThrow()
  expect(constructor).not.toHaveBeenCalled()
})
test('missing offline store fails explicitly', async () => {
  const { loaded } = fixture({ ContinuationRecordingStore: undefined })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toMatchObject({
    normalized: { code: 'capability.unsupported' }
  })
})

test('offline native class constructor failures use the canonical transport error', async () => {
  const { loaded } = fixture({
    ContinuationRecordingStore: class {
      constructor() {
        throw new Error('native constructor refused')
      }
    }
  })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toMatchObject({
    normalized: {
      code: 'platform.transport',
      operation: 'direct-gatt.recording-store',
      platform: { code: 'binding-rejection', safeMessage: 'native constructor refused' }
    }
  })
})

test('offline constructor preserves existing contract error identity', async () => {
  const { contractError } = require('../../../src/backend-contract/errors')
  const failure = contractError('permission.denied', 'platform', 'test.storage.construct')
  const { loaded } = fixture({
    ContinuationRecordingStore: class {
      constructor() {
        throw failure
      }
    }
  })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toBe(failure)
})

test('offline configure refuses unknown response fields', async () => {
  const original = fixture()
  const Base = original.loaded.module.ContinuationRecordingStore
  const { loaded } = fixture({
    ContinuationRecordingStore: class extends Base {
      async configureDirectory() {
        return JSON.stringify({ ok: true, value: { state: 'configured', encrypted: false, extra: true } })
      }
    }
  })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toMatchObject({
    normalized: { code: 'protocol.malformed' }
  })
})

test.each([
  [
    'malformed envelope',
    '{}',
    {
      code: 'protocol.malformed',
      operation: 'react-native-rust-core.wire.continuation.recording.configure.envelope.ok'
    }
  ],
  [
    'native failure without platform',
    JSON.stringify({
      ok: false,
      error: {
        code: 'permission.denied',
        domain: 'platform',
        operation: 'storage.configure',
        detail: 'directory refused',
        platform: null
      },
      commit: null,
      retryability: 'never'
    }),
    {
      code: 'permission.denied',
      domain: 'platform',
      operation: 'storage.configure',
      retryability: 'never',
      platform: { domain: 'ubm-desktop', code: 'permission.denied', safeMessage: 'directory refused', metadata: {} }
    }
  ],
  [
    'native failure with platform',
    JSON.stringify({
      ok: false,
      error: {
        code: 'platform.failure',
        domain: 'platform',
        operation: 'storage.configure',
        detail: 'directory refused',
        platform: {
          domain: 'sqlite',
          code: '14',
          message: 'cannot open',
          metadata: { extendedCode: 14 }
        }
      },
      commit: null,
      retryability: 'never'
    }),
    {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'storage.configure',
      retryability: 'never',
      platform: { domain: 'sqlite', code: '14', safeMessage: 'cannot open', metadata: { extendedCode: 14 } }
    }
  ]
])('offline configure preserves precise diagnostics: %s', async (_label, response, normalized) => {
  const Base = fixture().loaded.module.ContinuationRecordingStore
  const { loaded } = fixture({
    ContinuationRecordingStore: class extends Base {
      async configureDirectory() {
        return response
      }
    }
  })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toMatchObject({ normalized })
})
