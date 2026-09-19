const { BackendContractError } = require('../src/backend-contract/errors')
const { BleError } = require('../src/public/errors')
const { rehydratePublicError } = require('../src/public/error-bridge')

describe('public BleError', () => {
  test('owns direct public fields without inheriting the backend error class', () => {
    const error = new BleError('connection.failed', 'connection', 'public-errors.test')

    expect(error).toBeInstanceOf(Error)
    expect(error).not.toBeInstanceOf(BackendContractError)
    expect(error.name).toBe('BleError')
    expect(error.code).toBe('connection.failed')
    expect(error.domain).toBe('connection')
    expect(error.operation).toBe('public-errors.test')
    expect(error.platform).toBeNull()
    expect(error.recovery).toEqual({
      disposition: 'retry-with-backoff',
      actions: [{ kind: 'reconnect' }]
    })
  })

  test('copies and freezes platform metadata and recovery actions', () => {
    const metadataBytes = new Uint8Array([1, 2])
    const platform = {
      domain: 'test-host',
      code: 'E_TEST',
      safeMessage: 'safe test detail',
      metadata: { nested: { value: 'original' }, bytes: metadataBytes }
    }
    const error = new BleError('operation.timed-out', 'core', 'public-errors.test', { platform })

    expect(error.platform).not.toBe(platform)
    expect(error.platform).toEqual(platform)
    expect(Object.isFrozen(error.platform)).toBe(true)
    expect(Object.isFrozen(error.platform.metadata)).toBe(true)
    expect(Object.isFrozen(error.platform.metadata.nested)).toBe(true)
    expect(Object.isFrozen(error.recovery.actions[0])).toBe(true)
    expect(error.platform.metadata.bytes).not.toBe(metadataBytes)
    metadataBytes[0] = 99
    expect(error.platform.metadata.bytes[0]).toBe(1)
  })

  test('preserves unexpected errors at the public boundary', () => {
    const unexpected = new Error('unexpected')

    expect(rehydratePublicError(unexpected)).toBe(unexpected)
  })

  test('rejects unknown code and domain values before recovery lookup', () => {
    expect(() => new BleError('made-up', 'connection', 'public-errors.test')).toThrow('unknown BleError code')
    expect(() => new BleError('connection.failed', 'made-up', 'public-errors.test')).toThrow('unknown BleError domain')
    expect(() => new BleError('connection.failed', 'connection', '')).toThrow('operation must be non-empty')
  })
})

// PR210-35: the public error reports the operation's own retryability so a
// public caller makes the same decision the core made.
describe('public BleError retryability', () => {
  const { commitUncertainError, contractError } = require('../src/backend-contract/errors')

  test('defaults retryability from the code for errors built without an answer', () => {
    expect(new BleError('operation.timed-out', 'core', 'public-errors.test').retryability).toBe('caller-decides')
    expect(new BleError('operation.aborted', 'core', 'public-errors.test').retryability).toBe('caller-decides')
    expect(new BleError('connection.failed', 'connection', 'public-errors.test').retryability).toBe('never')
  })

  test('rehydrates a dispatched write that timed out as never retryable with verify-state advice', () => {
    for (const code of ['operation.timed-out', 'operation.aborted']) {
      const error = rehydratePublicError(commitUncertainError(code, 'core', 'core.gatt.write'))

      expect(error).toBeInstanceOf(BleError)
      expect(error.code).toBe(code)
      expect(error.retryability).toBe('never')
      expect(error.recovery).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] })
      expect(Object.isFrozen(error.recovery.actions[0])).toBe(true)
    }
  })

  test('rehydrates an undispatched timeout as caller-decides with retry advice', () => {
    const error = rehydratePublicError(contractError('operation.timed-out', 'core', 'core.gatt.write'))

    expect(error.retryability).toBe('caller-decides')
    expect(error.recovery).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'retry', afterMs: null }] })
  })

  test('accepts an explicit retryability and rejects an unknown one', () => {
    const error = new BleError('operation.timed-out', 'core', 'public-errors.test', { retryability: 'never' })
    expect(error.retryability).toBe('never')
    expect(error.recovery.actions).toEqual([{ kind: 'verify-state' }])
    expect(
      () => new BleError('operation.timed-out', 'core', 'public-errors.test', { retryability: 'sometimes' })
    ).toThrow('unknown BleError retryability')
  })
})

// PR210-35 end to end: a public write that was dispatched and then timed out
// reaches the application as a BleError that is never retryable and advises
// verifying state, while a read that timed out keeps its retry advice.
describe('public write timeout after dispatch', () => {
  const {
    attachBleBackend,
    BleManager: InternalBleManager,
    createManagerOwnershipAuthority,
    DEFAULT_BLE_MANAGER_OPTIONS
  } = require('../src/manager/ble-manager')
  const { createPublicBleManager } = require('../src/public/ble-manager')
  const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
  const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')

  function range(axis) {
    return versionRange(version(axis, 1), version(axis, 1))
  }

  async function publicFixture() {
    const fixture = createDeterministicTestBackend()
    const attachedBackend = await attachBleBackend(fixture.backend, {
      backendContract: range('backend-contract'),
      capabilitySchema: range('capability-schema'),
      eventSchema: range('event-schema'),
      traceFormat: range('trace-format')
    })
    const internal = await InternalBleManager.create(
      {
        attachedBackend,
        clientId: opaqueId('public-errors-client', 'client', 'public-errors'),
        managerId: opaqueId('public-errors-manager', 'manager', 'public-errors'),
        ownerMode: 'owning'
      },
      createManagerOwnershipAuthority(attachedBackend),
      DEFAULT_BLE_MANAGER_OPTIONS
    )
    const manager = await createPublicBleManager(internal, () => Number(fixture.controller.clock.now()))
    return { fixture, manager }
  }

  async function settle(fixture, promise) {
    let settled = false
    void promise.then(
      () => (settled = true),
      () => (settled = true)
    )
    for (let attempt = 0; attempt < 40 && !settled; attempt += 1) {
      fixture.controller.clock.runUntilIdle()
      await Promise.resolve()
    }
    return promise
  }

  const slowCompletion = { delayMs: 100, failure: null, cancellable: false, deadlineOrder: 'deadline-first' }

  async function dispatchThenAbort(fixture, stage, start) {
    fixture.controller.queueCompletion(stage, slowCompletion)
    const abort = new AbortController()
    const pending = start(abort.signal).catch(error => error)
    for (let attempt = 0; attempt < 20; attempt += 1) {
      fixture.controller.clock.advanceBy(0)
      await Promise.resolve()
    }
    expect(fixture.controller.traceSnapshot().at(-1)).toMatchObject({ event: 'dispatched' })
    abort.abort()
    return settle(fixture, pending)
  }

  test('a dispatched write that is aborted is never retryable and advises verify-state', async () => {
    const { fixture, manager } = await publicFixture()
    const connection = await settle(fixture, manager.connect('deterministic-peer'))
    const database = await settle(fixture, connection.discover())
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')

    const failure = await dispatchThenAbort(fixture, 'write', signal =>
      characteristic.write(new Uint8Array([1]), { signal })
    )
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.code).toBe('operation.aborted')
    expect(failure.retryability).toBe('never')
    expect(failure.recovery).toEqual({ disposition: 'caller-policy', actions: [{ kind: 'verify-state' }] })

    const readFailure = await dispatchThenAbort(fixture, 'read', signal => characteristic.read({ signal }))
    expect(readFailure).toBeInstanceOf(BleError)
    expect(readFailure.code).toBe('operation.aborted')
    expect(readFailure.retryability).toBe('caller-decides')
    expect(readFailure.recovery.actions).toEqual([{ kind: 'retry', afterMs: null }])

    await settle(fixture, manager.destroy())
  })
})

// PR210-37: the commit state rides the normalized error across host
// boundaries only when its owner stated it.
test('a dispatched write that may have committed says so in the normalized error', () => {
  const { commitUncertainError, contractError, serializeNormalizedError } = require('../src/backend-contract/errors')
  const uncertain = commitUncertainError('operation.timed-out', 'gatt', 'core.gatt.write').normalized
  expect(uncertain).toMatchObject({ retryability: 'never', commit: 'uncertain' })
  expect(serializeNormalizedError(uncertain)).toMatchObject({ commit: 'uncertain' })
  const unstated = contractError('gatt.read-failed', 'gatt', 'core.gatt.read').normalized
  expect('commit' in unstated).toBe(false)
  expect('commit' in serializeNormalizedError(unstated)).toBe(false)
  expect(serializeNormalizedError({ ...unstated, commit: null })).toMatchObject({ commit: null })
})

// PR210-42: the public error carries the owner's commit state and follows it.
describe('public BleError commit', () => {
  const { contractError } = require('../src/backend-contract/errors')

  test('defaults to null and keeps every existing field', () => {
    const error = new BleError('connection.failed', 'connection', 'public-errors.test')
    expect(error.commit).toBeNull()
    expect(error.retryability).toBe('never')
    expect(error.recovery).toEqual({ disposition: 'retry-with-backoff', actions: [{ kind: 'reconnect' }] })
  })

  test('rehydrates an uncertain commit on any code as verify-state, never replay', () => {
    const backend = new BackendContractError({
      ...contractError('operation.disconnected', 'gatt', 'tauri.gatt.write').normalized,
      commit: 'uncertain'
    })
    const error = rehydratePublicError(backend)

    expect(error).toBeInstanceOf(BleError)
    expect(error.commit).toBe('uncertain')
    expect(error.retryability).toBe('never')
    expect(error.recovery).toEqual({
      disposition: 'caller-policy',
      actions: [{ kind: 'reconnect' }, { kind: 'verify-state' }]
    })
  })

  test('rehydrates not-dispatched and null commit states unchanged', () => {
    const notDispatched = rehydratePublicError(
      new BackendContractError({
        ...contractError('operation.timed-out', 'gatt', 'tauri.gatt.write').normalized,
        commit: 'not-dispatched'
      })
    )
    expect(notDispatched.commit).toBe('not-dispatched')
    expect(notDispatched.retryability).toBe('caller-decides')
    expect(notDispatched.recovery.actions).toEqual([{ kind: 'retry', afterMs: null }])

    const unknown = rehydratePublicError(
      new BackendContractError({
        ...contractError('gatt.read-failed', 'gatt', 'tauri.gatt.read').normalized,
        commit: null
      })
    )
    expect(unknown.commit).toBeNull()
  })

  test('rejects an unknown commit value', () => {
    expect(() => new BleError('operation.timed-out', 'core', 'public-errors.test', { commit: 'maybe' })).toThrow(
      'unknown BleError commit'
    )
  })
})

// PR210-42: a cleanup failure keeps the owner's commit state instead of
// dropping it at the public projection.
describe('public cleanup failure commit', () => {
  const { toPublicCleanupRecord } = require('../src/public/cleanup')
  const failure = commit => ({
    state: 'release-failed',
    failures: [
      {
        resourceKind: 'subscription',
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation: 'cleanup.test',
          platform: null,
          retryability: 'never',
          ...(commit === undefined ? {} : { commit })
        }
      }
    ]
  })

  test('carries a reported commit state and omits an absent one', () => {
    expect(toPublicCleanupRecord(failure('uncertain')).failures[0].error.commit).toBe('uncertain')
    expect(toPublicCleanupRecord(failure(null)).failures[0].error.commit).toBeNull()
    expect(toPublicCleanupRecord(failure(undefined)).failures[0].error).not.toHaveProperty('commit')
  })

  test('rejects an unknown commit state', () => {
    expect(() => toPublicCleanupRecord(failure('maybe'))).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'protocol.malformed' }) })
    )
  })
})
