const { createWebRecordingPartition } = require('unified-ble-manager/web')
const { admissionLocks } = require('./web-clear-admission-locks-helper')

const flush = async () => {
  for (let i = 0; i < 100; i++) await Promise.resolve()
}

test.each([
  ['QuotaExceededError', 'InvalidStateError', 'stream.quota'],
  ['InvalidStateError', 'QuotaExceededError', 'capability.unavailable']
])('public admission classifies %s and preserves the failed %s abort until settlement', async (queueName, abortName, code) => {
  jest.useFakeTimers()
  const transactions = []
  const queueError = new DOMException('queue failed', queueName)
  queueError.cause = new Error('underlying queue cause')
  const abortError = new DOMException('abort failed', abortName)
  const database = {
    name: 'admission-error-provenance',
    transaction() {
      let used = false
      const tx = {
        error: null,
        onabort: null,
        oncomplete: null,
        abort() { throw abortError },
        objectStore() {
          used = true
          if (transactions.filter(item => item.used).length === 1) throw queueError
          return { get: () => ({ onsuccess: null }) }
        },
        get used() { return used }
      }
      transactions.push(tx)
      queueMicrotask(() => { if (!used) tx.oncomplete?.() })
      return tx
    }
  }
  try {
    const partition = createWebRecordingPartition({
      database,
      locks: admissionLocks(),
      clearAdmissionLimit: 1,
      crypto: { getRandomValues: bytes => bytes },
      digest: async () => 'a'.repeat(64)
    })
    const controller = await partition.createController()
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }
    const first = controller.admitClearAttemptV2(input).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(0)
    await flush()
    expect(await first).toMatchObject({
      code,
      platform: {
        domain: 'indexeddb',
        code: 'AggregateError',
        metadata: {
          errors: [
            { name: queueName, cause: { name: 'Error' } },
            { name: abortName }
          ]
        }
      }
    })
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)
    await expect(controller.admitClearAttemptV2(input)).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)

    transactions.find(tx => tx.used).onabort()
    await flush()
    const retry = controller.admitClearAttemptV2(input).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(0)
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    transactions.filter(tx => tx.used)[1].onabort()
    await retry
  } finally {
    for (const tx of transactions) if (tx.used) tx.onabort?.()
    jest.useRealTimers()
  }
})


function failingQueueDatabase(queueError, abortError, name) {
  const transactions = []
  return {
    name,
    transactions,
    transaction() {
      let used = false
      const tx = {
        error: null, onabort: null, oncomplete: null,
        abort() { throw abortError },
        objectStore() { used = true; throw queueError },
        get used() { return used }
      }
      transactions.push(tx)
      queueMicrotask(() => { if (!used) tx.oncomplete?.() })
      return tx
    }
  }
}

async function admissionFailure(database) {
  const partition = createWebRecordingPartition({
    database, locks: admissionLocks(), clearAdmissionLimit: 1,
    crypto: { getRandomValues: bytes => bytes }, digest: async () => 'a'.repeat(64)
  })
  const controller = await partition.createController()
  const error = await controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }).catch(value => value)
  database.transactions.filter(tx => tx.used).forEach(tx => tx.onabort?.())
  return error
}

test('queue errors redact arbitrary messages, names and primitive values through the public controller', async () => {
  const primary = new Error('token=queue-secret', { cause: 'client-secret' })
  primary.name = 'PrivateOwnerSecret'
  const secondary = new Error('token=abort-secret')
  const error = await admissionFailure(failingQueueDatabase(primary, secondary, 'redaction'))
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { metadata: { errors: [{ name: 'UnknownError', cause: { name: 'UnknownError' } }, { name: 'Error' }] } } })
  expect(JSON.stringify(error)).not.toMatch(/secret|PrivateOwner/)
})

test.each([
  ['null prototype', () => Object.create(null)],
  ['throwing properties', () => Object.defineProperties({}, Object.fromEntries(['name', 'message', 'errors', 'cause'].map(key => [key, { get() { throw new Error('getter-secret') } }])) )],
  ['revoked proxy', () => { const { proxy, revoke } = Proxy.revocable({}, {}); revoke(); return proxy }],
  ['hostile proxy', () => new Proxy({}, { get() { throw new Error('getter-secret') }, has() { throw new Error('has-secret') }, getPrototypeOf() { throw new Error('prototype-secret') } })]
])('public controller handles %s thrown during queue and abort', async (_label, create) => {
  const error = await admissionFailure(failingQueueDatabase(create(), create(), `hostile-${_label}`))
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { domain: 'indexeddb', metadata: { errors: [{ name: 'UnknownError' }, { name: 'UnknownError' }] } } })
  expect(JSON.stringify(error)).not.toContain('secret')
})

test('nested cyclic and huge error graphs have a single bounded JSON projection with omission', async () => {
  const secret = '\0'.repeat(2048) + 'hidden-secret'
  const root = new AggregateError([], secret)
  let node = root
  for (let i = 0; i < 200; i++) {
    node.cause = new AggregateError(Array.from({ length: 100 }, () => new Error(secret)), secret)
    node = node.cause
  }
  node.cause = root
  const error = await admissionFailure(failingQueueDatabase(root, root, 'huge-cyclic'))
  const json = JSON.stringify(error)
  expect(error).toMatchObject({ code: 'capability.unavailable' })
  expect(json).toContain('omitted')
  expect(json).not.toContain('hidden-secret')
  expect(Buffer.byteLength(json, 'utf8')).toBeLessThan(1048576)
  expect(() => require('../../src/backend-contract/errors').assertPlatformFailureEnvelopeBound(error, 'test')).not.toThrow()
})

test.each(['oncomplete', 'onabort'])('guarded quota and abort failure retain classification and the slot until %s', async terminal => {
  global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) }
  const transactions = []
  const database = {
    name: `guarded-${terminal}`,
    transaction() {
      let used = false
      const tx = {
        error: null, oncomplete: null, onabort: null,
        abort() { throw new DOMException('abort-secret', 'InvalidStateError') },
        objectStore() {
          used = true
          const request = value => {
            const result = { result: value, onsuccess: null }
            queueMicrotask(() => result.onsuccess?.())
            return result
          }
          return {
            get(key) {
              if (key === ':ubm-staged-proof-ledger-v1') return request({ recordingId: key, version: 1, epoch: 'a'.repeat(64), watermark: '1', revision: '0', retained: 1, requestCount: 0, limit: 1 })
              if (key === ':ubm-staged-proof-ledger-marker-v1') return request({ recordingId: key, version: 1, epoch: 'a'.repeat(64) })
              return request(undefined)
            },
            count: () => request(0)
          }
        },
        get used() { return used }
      }
      transactions.push(tx)
      queueMicrotask(() => { if (!used) tx.oncomplete?.() })
      return tx
    }
  }
  try {
    const partition = createWebRecordingPartition({ database, locks: admissionLocks(), clearAdmissionLimit: 1, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'a'.repeat(64) })
    const controller = await partition.createController()
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'b'.repeat(64) }
    let done = false
    const first = controller.admitClearAttemptV2(input).catch(error => error).finally(() => { done = true })
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)
    expect(done).toBe(false)
    await expect(controller.admitClearAttemptV2(input)).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)
    transactions.find(tx => tx.used)[terminal]()
    const error = await first
    expect(error).toMatchObject({ code: 'stream.quota', operation: 'web-recording.staged-proof-capacity', platform: { metadata: { errors: [{ name: 'BackendContractError' }, { name: 'InvalidStateError' }] } } })
    expect(JSON.stringify(error)).not.toContain('abort-secret')
    await flush()
    const retry = controller.admitClearAttemptV2(input).catch(error => error)
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    transactions.filter(tx => tx.used)[1][terminal]()
    await retry
  } finally {
    transactions.filter(tx => tx.used).forEach(tx => tx.onabort?.())
    delete global.IDBKeyRange
  }
})

test('cyclic causes retain explicit omission and redact each message', async () => {
  const primary = new Error('secret in primary')
  const child = new Error('secret in child', { cause: primary })
  primary.cause = child
  const error = await admissionFailure(failingQueueDatabase(primary, new Error('abort-secret'), 'cycle'))
  expect(error.platform.metadata.errors[0]).toEqual({ name: 'Error', cause: { name: 'Error', cause: { name: 'Error', omitted: 'cyclic error' } } })
  expect(JSON.stringify(error)).not.toContain('secret')
})

test.each([
  ['VersionError', 'protocol.incompatible'],
  ['QuotaExceededError', 'stream.quota'],
  ['SecurityError', 'capability.unavailable']
])('direct %s mapping remains intact', async (name, code) => {
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({
    database: { transaction() { throw new DOMException('secret', name) } }
  })
  await expect(controller.clearProofUsageV2()).rejects.toMatchObject({ code, platform: { code: name } })
})

test('direct revoked proxy from transaction creation still becomes a public error', async () => {
  const { proxy, revoke } = Proxy.revocable({}, {})
  revoke()
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({
    database: { transaction() { throw proxy } }
  })
  await expect(controller.clearProofUsageV2()).rejects.toMatchObject({ code: 'capability.unavailable', platform: { code: 'UnknownError' } })
})

test('a proxy impersonating a typed error cannot interrupt evidence projection', async () => {
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const primary = new Proxy(Object.create(BackendContractError.prototype), { get() { throw new Error('secret') } })
  const error = await admissionFailure(failingQueueDatabase(primary, new Error('abort-secret'), 'typed-proxy'))
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { metadata: { errors: [{ name: 'UnknownError' }, { name: 'Error' }] } } })
})

test('a proxy impersonating the combined failure cannot escape the public boundary', async () => {
  const { WebIndexedDbQueueAbortError } = require('../../src/web/web-indexeddb-transaction')
  const proxy = new Proxy(Object.create(WebIndexedDbQueueAbortError.prototype), { get() { throw new Error('secret') } })
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({
    database: { transaction() { throw proxy } }
  })
  await expect(controller.clearProofUsageV2()).rejects.toMatchObject({ code: 'capability.unavailable', platform: { domain: 'indexeddb' } })
})

test('a proxy impersonating a typed failure before transaction start cannot escape the public boundary', async () => {
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const proxy = new Proxy(Object.create(BackendContractError.prototype), { get() { throw new Error('secret') } })
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({
    database: { transaction() { throw proxy } }
  })
  await expect(controller.clearProofUsageV2()).rejects.toMatchObject({ code: 'capability.unavailable', platform: { domain: 'indexeddb' } })
})

const { BackendContractError, contractError } = require('../../src/backend-contract/errors')
const hostTypedFailures = [
  ['prototype spoof', () => Object.assign(Object.create(BackendContractError.prototype), {
    normalized: { code: 'stream.quota', domain: 'restoration', operation: 'web-recording.token-secret', retryability: 'never', platform: { domain: 'host-secret', code: 'code-secret', safeMessage: 'message-secret', metadata: { key: 'value-secret' } } }
  })],
  ['genuine host-created typed error', () => contractError('stream.quota', 'restoration', 'web-recording.token-secret', { domain: 'host-secret', code: 'code-secret', safeMessage: 'message-secret', metadata: { key: 'value-secret' } })],
  ['proxy around typed error', () => new Proxy(contractError('stream.quota', 'restoration', 'web-recording.token-secret', { domain: 'host-secret', code: 'code-secret', safeMessage: 'message-secret', metadata: {} }), {})]
]

test.each(hostTypedFailures)('direct %s from host is sanitized', async (_label, create) => {
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({
    database: { transaction() { throw create() } }
  })
  const error = await controller.clearProofUsageV2().catch(value => value)
  expect(error).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.indexeddb-transaction', platform: { domain: 'indexeddb' } })
  expect(`${error.message} ${JSON.stringify(error)}`).not.toContain('secret')
})

test.each(hostTypedFailures)('combined %s from host is sanitized', async (_label, create) => {
  const error = await admissionFailure(failingQueueDatabase(create(), new Error('abort-secret'), `typed-${_label}`))
  expect(error).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.indexeddb-transaction', platform: { domain: 'indexeddb', code: 'AggregateError' } })
  expect(`${error.message} ${JSON.stringify(error)}`).not.toContain('secret')
})

test('direct library-generated staged proof quota retains its typed classification', async () => {
  global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) }
  const database = {
    name: 'direct-library-quota',
    transaction() {
      let used = false
      const tx = {
        error: null, oncomplete: null, onabort: null,
        abort() { queueMicrotask(() => tx.onabort?.()) },
        objectStore() {
          used = true
          const request = value => {
            const result = { result: value, onsuccess: null }
            queueMicrotask(() => result.onsuccess?.())
            return result
          }
          return {
            get(key) {
              if (key === ':ubm-staged-proof-ledger-v1') return request({ recordingId: key, version: 1, epoch: 'a'.repeat(64), watermark: '1', revision: '0', retained: 1, requestCount: 0, limit: 1 })
              if (key === ':ubm-staged-proof-ledger-marker-v1') return request({ recordingId: key, version: 1, epoch: 'a'.repeat(64) })
              return request(undefined)
            },
            count: () => request(0)
          }
        }
      }
      queueMicrotask(() => { if (!used) tx.oncomplete?.() })
      return tx
    }
  }
  try {
    const partition = createWebRecordingPartition({ database, locks: admissionLocks(), clearAdmissionLimit: 1, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'a'.repeat(64) })
    const controller = await partition.createController()
    await expect(controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: 'producer', requestId: 'b'.repeat(64) })).rejects.toMatchObject({ code: 'stream.quota', operation: 'web-recording.staged-proof-capacity' })
  } finally {
    delete global.IDBKeyRange
  }
})

test('recording error origin uses identity and an independent snapshot', () => {
  const { webRecordingContractError, trustedWebRecordingError } = require('../../src/web/web-recording-errors')
  const original = webRecordingContractError('stream.quota', 'restoration', 'web-recording.staged-proof-capacity', {
    domain: 'indexeddb', code: 'known', safeMessage: 'fixed', metadata: { nested: { value: 'safe' } }
  })
  original.normalized.operation = 'web-recording.mutated-secret'
  original.normalized.platform.metadata.nested.value = 'mutated-secret'
  expect(trustedWebRecordingError(original).normalized).toMatchObject({ operation: 'web-recording.staged-proof-capacity', platform: { metadata: { nested: { value: 'safe' } } } })
  const returned = trustedWebRecordingError(original)
  returned.normalized.operation = 'returned-secret'
  returned.normalized.platform.metadata.nested.value = 'returned-secret'
  expect(trustedWebRecordingError(original).normalized).toMatchObject({ operation: 'web-recording.staged-proof-capacity', platform: { metadata: { nested: { value: 'safe' } } } })
  expect(trustedWebRecordingError(new Proxy(original, {}))).toBeUndefined()
  expect(trustedWebRecordingError(Object.create(Object.getPrototypeOf(original)))).toBeUndefined()
  const revoked = Proxy.revocable(original, {})
  revoked.revoke()
  expect(trustedWebRecordingError(revoked.proxy)).toBeUndefined()
})

test.each(hostTypedFailures)('public recovery inspector redacts %s', async (_label, create) => {
  const { createWebRecordingRecoveryInspector } = require('../../src/web')
  const partition = createWebRecordingPartition({ database: { transaction() { throw create() } }, locks: admissionLocks(), crypto: { getRandomValues: b => b }, digest: async () => 'a'.repeat(64) })
  const inspector = await createWebRecordingRecoveryInspector(partition)
  const error = await inspector.listClearProofs({ after: null, limit: 1 }).catch(value => value)
  expect(error).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.indexeddb-transaction', platform: { domain: 'indexeddb' } })
  expect(`${error.message} ${JSON.stringify(error)}`).not.toContain('secret')
})

test('public recovery inspector preserves a library validation failure', async () => {
  const { createWebRecordingRecoveryInspector } = require('../../src/web')
  const partition = createWebRecordingPartition({ database: { transaction() { throw new Error('unexpected') } }, locks: admissionLocks(), crypto: { getRandomValues: b => b }, digest: async () => 'a'.repeat(64) })
  const inspector = await createWebRecordingRecoveryInspector(partition)
  await expect(inspector.listClearProofs({ after: null, limit: 0 })).rejects.toMatchObject({ code: 'lifecycle.invariant-violation', operation: 'web-recording.recovery-page' })
})

test('public attempt inspection does not classify an exception from a hostile prototype trap', async () => {
  const { createWebRecordingRecoveryInspector } = require('../../src/web')
  const hostile = new Proxy({}, { getPrototypeOf() { throw new DOMException('trap-secret', 'QuotaExceededError') } })
  const partition = createWebRecordingPartition({ database: { transaction() { throw hostile } }, locks: admissionLocks(), crypto: { getRandomValues: b => b }, digest: async () => 'a'.repeat(64) })
  const inspector = await createWebRecordingRecoveryInspector(partition)
  const error = await inspector.getClearAttemptV1({ version: 1, recordingId: 'sample', producerIncarnation: 'producer', attemptId: 'a'.repeat(64) }).catch(value => value)
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { code: 'UnknownError' } })
  expect(JSON.stringify(error)).not.toContain('secret')
})

test('public recovery inspector retains redacted queue and abort evidence', async () => {
  const { createWebRecordingRecoveryInspector } = require('../../src/web')
  const database = { transaction() {
    const tx = { error: null, oncomplete: null, onabort: null, objectStore() { throw new DOMException('queue-secret', 'QuotaExceededError') }, abort() { throw new DOMException('abort-secret', 'InvalidStateError') } }
    queueMicrotask(() => tx.oncomplete?.())
    return tx
  } }
  const inspector = await createWebRecordingRecoveryInspector(createWebRecordingPartition({ database, locks: admissionLocks(), crypto: { getRandomValues: b => b }, digest: async () => 'a'.repeat(64) }))
  const error = await inspector.listClearProofs({ after: null, limit: 1 }).catch(value => value)
  expect(error).toMatchObject({ code: 'stream.quota', platform: { code: 'AggregateError', metadata: { errors: [{ name: 'QuotaExceededError' }, { name: 'InvalidStateError' }] } } })
  expect(JSON.stringify(error)).not.toContain('secret')
})

test('forged combined transaction wrapper cannot manufacture quota or abort evidence', async () => {
  const { WebIndexedDbQueueAbortError } = require('../../src/web/web-indexeddb-transaction')
  const forged = Object.assign(Object.create(WebIndexedDbQueueAbortError.prototype), {
    queueError: { name: 'QuotaExceededError' }, errors: [{ name: 'QuotaExceededError' }, { name: 'SecurityError' }]
  })
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({ database: { transaction() { throw forged } } })
  const error = await controller.clearProofUsageV2().catch(value => value)
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { domain: 'indexeddb', metadata: {} } })
})

test('combined failure keeps its captured classification and evidence after mutation', async () => {
  const { webIndexedDbQueueAbortError } = require('../../src/web/web-recording-errors')
  const primary = new Error('primary-secret')
  primary.name = 'SecurityError'
  const abort = new Error('abort-secret')
  abort.name = 'InvalidStateError'
  const combined = webIndexedDbQueueAbortError(primary, abort)
  primary.name = 'QuotaExceededError'
  abort.name = 'QuotaExceededError'
  combined.queueError = new DOMException('forged-secret', 'QuotaExceededError')
  combined.errors = [combined.queueError]
  const controller = await require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController({ database: { transaction() { throw combined } } })
  const error = await controller.clearProofUsageV2().catch(value => value)
  expect(error).toMatchObject({ code: 'capability.unavailable', platform: { code: 'AggregateError', metadata: { errors: [{ name: 'SecurityError' }, { name: 'InvalidStateError' }] } } })
  expect(JSON.stringify(error)).not.toContain('secret')
})
