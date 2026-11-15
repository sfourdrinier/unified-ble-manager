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
test('offline ordered stop uses the addon receipt offer without opening a radio', async () => {
  const Base = fixture().loaded.module.ContinuationRecordingStore
  const receipt = { recording: { version: 2, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' } }
  class OrderedStore extends Base {
    async stopOrdered(id) {
      expect(id).toBe('sample')
      return JSON.stringify({ ok: true, value: receipt })
    }
  }
  const { loaded } = fixture({
    ContinuationRecordingStore: OrderedStore,
    orderedRecordingReceiptVersion() { expect(this).toBe(loaded.module); return 2 }
  })
  const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')
  const controller = await openNativeContinuationRecordings(bindDesktopCore(host, loaded), '/private/app')
  expect(await controller.stopOrdered('sample')).toEqual(receipt)
})

test.each([undefined, 1, '2'])('offline loader does not assume v2 for offer %s', async offer => {
  const stopOrdered = jest.fn()
  const Base = fixture().loaded.module.ContinuationRecordingStore
  class OrderedStore extends Base { async stopOrdered(id) { return stopOrdered(id) } }
  const { loaded } = fixture({
    ContinuationRecordingStore: OrderedStore,
    orderedRecordingReceiptVersion: offer === undefined ? undefined : () => offer
  })
  const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')
  const controller = await openNativeContinuationRecordings(bindDesktopCore(host, loaded), '/private/app')
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'protocol.incompatible' })
  expect(stopOrdered).not.toHaveBeenCalled()
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
  'platform-owner-private',
  'host-protected',
  'windows-current-user-protected-dacl',
  'capability.unsupported'
])('offline configure accepts the explicit scan-recovery storage capability %s', async scanRecoveryStorage => {
  const Base = fixture().loaded.module.ContinuationRecordingStore
  const { loaded } = fixture({
    ContinuationRecordingStore: class extends Base {
      async configureDirectory() {
        return JSON.stringify({
          ok: true,
          value: { state: 'configured', encrypted: false, scanRecoveryStorage }
        })
      }
    }
  })
  await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).resolves.toBeDefined()
})

test.each(['unknown-storage-protector', 17, null, { toString: () => { throw new Error('must not coerce') } }])(
  'offline configure rejects invalid scan-recovery storage capability %p',
  async scanRecoveryStorage => {
    const Base = fixture().loaded.module.ContinuationRecordingStore
    const { loaded } = fixture({
      ContinuationRecordingStore: class extends Base {
        async configureDirectory() {
          return JSON.stringify({
            ok: true,
            value: { state: 'configured', encrypted: false, scanRecoveryStorage }
          })
        }
      }
    })
    await expect(bindDesktopCore(host, loaded).openRecordingStore('/private/app')).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'continuation.recording.configure' }
    })
  }
)

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

test('offline exact legacy controls require the complete versioned surface and preserve receiver', async () => {
  const original = fixture()
  const Base = original.loaded.module.ContinuationRecordingStore
  class ExactStore extends Base {
    async legacyRecordingPrepareExact(id, items, bytes) { this.calls.push(['legacy', id, items, bytes]); return '{}' }
    async legacyRecordingAcknowledgeExact(id, page) { this.calls.push(['ack', id, page]); return '{}' }
    async legacyRecordingRetireExact(id, page) { this.calls.push(['retire', id, page]); return '{}' }
    async legacyRecordingConfirmExactRetirement(id, page) { this.calls.push(['confirm', id, page]); return '{}' }
    async inspectRawRecord(id, source, ordinal, offset, maxBytes) { this.calls.push(['inspect', id, source, ordinal, offset, maxBytes]); return '{}' }
  }
  const { loaded } = fixture({ ContinuationRecordingStore: ExactStore })
  const calls = original.calls
  const store = await bindDesktopCore(host, loaded).openRecordingStore('/private/app')
  await store.legacyRecordingPrepareExact('sample', 1, 10)
  await store.legacyRecordingAcknowledgeExact('sample', '{}')
  await store.legacyRecordingRetireExact('sample', '{}')
  await store.legacyRecordingConfirmExactRetirement('sample', '{}')
  await store.inspectRawRecord('sample', 'records', null, '0', 8)
  expect(calls.slice(1)).toEqual([['legacy', 'sample', 1, 10], ['ack', 'sample', '{}'], ['retire', 'sample', '{}'], ['confirm', 'sample', '{}'], ['inspect', 'sample', 'records', null, '0', 8]])
  class IncompleteStore extends Base { async legacyRecordingPrepareExact() { return '{}' } }
  const incomplete = fixture({ ContinuationRecordingStore: IncompleteStore })
  expect((await bindDesktopCore(host, incomplete.loaded).openRecordingStore('/private/app')).legacyRecordingPrepareExact).toBeUndefined()
})

test.each([
  'legacyRecordingPrepareExact',
  'legacyRecordingAcknowledgeExact',
  'legacyRecordingRetireExact',
  'legacyRecordingConfirmExactRetirement'
])('offline loader withholds all exact controls when %s is missing', async missing => {
  const Base = fixture().loaded.module.ContinuationRecordingStore
  class PartialStore extends Base {
    async legacyRecordingPrepareExact() { return '{}' }
    async legacyRecordingAcknowledgeExact() { return '{}' }
    async legacyRecordingRetireExact() { return '{}' }
    async legacyRecordingConfirmExactRetirement() { return '{}' }
  }
  Object.defineProperty(PartialStore.prototype, missing, { value: undefined })
  const { loaded } = fixture({ ContinuationRecordingStore: PartialStore })
  const store = await bindDesktopCore(host, loaded).openRecordingStore('/private/app')
  for (const method of ['legacyRecordingPrepareExact', 'legacyRecordingAcknowledgeExact', 'legacyRecordingRetireExact', 'legacyRecordingConfirmExactRetirement']) {
    expect(store[method]).toBeUndefined()
  }
  expect(typeof store.prepare).toBe('function')
  expect(typeof store.acknowledge).toBe('function')
})

if (process.env.UBM_NAPI_TEST_ADDON) test('real native store exports damaged bytes through loader and desktop wrapper', async () => {
  const fs = require('node:fs')
  const os = require('node:os')
  const path = require('node:path')
  const { execFileSync } = require('node:child_process')
  const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')
  const addon = require(process.env.UBM_NAPI_TEST_ADDON)
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-wrapper-raw-'))
  try {
    execFileSync('python3', ['-c', `import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.executescript("PRAGMA application_id=1430408522; PRAGMA user_version=2; CREATE TABLE records(ordinal INTEGER PRIMARY KEY,body TEXT NOT NULL,bytes INTEGER NOT NULL); INSERT INTO records VALUES(1,CAST(X'ff7b' AS TEXT),3);"); c.close()`, path.join(dir, 'sample.sqlite')])
    const { loaded } = fixture({ ContinuationRecordingStore: addon.ContinuationRecordingStore })
    const recordings = await openNativeContinuationRecordings(bindDesktopCore(host, loaded), dir)
    const chunk = await recordings.inspectRawRecord('sample', 'records', '1', '0', 8)
    expect(chunk.bytes).toEqual([255, 123])
    expect(chunk.lengthValid).toBe(false)
    expect(chunk.lengthFault.kind).toBe('storage.corrupt')
    expect(chunk.jsonValid).toBe(false)
  } finally { fs.rmSync(dir, { recursive: true, force: true }) }
})
