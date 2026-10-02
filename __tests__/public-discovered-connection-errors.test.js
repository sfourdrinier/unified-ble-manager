const { createPublicBleManager } = require('../src/public/ble-manager')
const { IpcPublicManagerAdapter } = require('../src/ipc/public-manager')
const { BleError } = require('../src/public/errors')
const { BleCleanupError } = require('../src/public/error-bridge')
const { contractError } = require('../src/backend-contract/errors')
const { opaqueId } = require('../src/backend-contract/primitives')

const released = { state: 'released', failures: [] }
const failedCleanup = {
  state: 'release-failed',
  failures: [
    {
      resourceKind: 'connection',
      error: {
        code: 'connection.lost',
        domain: 'connection',
        operation: 'helper-test.release',
        platform: null,
        retryability: 'never'
      }
    }
  ]
}

function emptyEvents() {
  return {
    [Symbol.asyncIterator]: () => ({ next: async () => ({ done: true, value: undefined }) })
  }
}

async function fixture(
  host,
  clock,
  { onConnect = () => {}, onDiscover = () => {}, cleanup = released, cleanupError } = {}
) {
  const calls = { connect: [], discover: [], release: 0 }
  const path = {
    attachment: {},
    attachmentId: 'attachment-1',
    peerId: 'peer-1',
    connectionId: 'connection-1',
    ownerLeaseId: 'lease-1',
    connectionGeneration: 'connection-generation-1',
    databaseId: 'database-1',
    databaseGeneration: 'generation-1'
  }
  const base = {
    handle: 'connection-handle-1',
    ...path,
    events: emptyEvents(),
    discover: async options => {
      calls.discover.push(options)
      await onDiscover(options)
      return {
        path,
        assertCurrent: () => {},
        snapshot: async () => ({ path, services: [], characteristics: [], descriptors: [] })
      }
    },
    release: async () => {
      calls.release += 1
      if (cleanupError !== undefined) throw cleanupError
      return cleanup
    }
  }
  const direct = { id: 'connection:direct', state: 'supported', limitations: [] }
  const capabilities = {
    supports: id => id === direct.id,
    get: id => (id === direct.id ? direct : undefined),
    require: () => direct,
    list: () => [direct]
  }
  const connect = async (_peer, options) => {
    calls.connect.push(options)
    await onConnect()
    return base
  }
  const manager =
    host === 'IPC'
      ? new IpcPublicManagerAdapter(
          { capabilities, bootstrap: { discovery: { kind: 'continuous-scan' } }, connect },
          {
            capabilities,
            adapter: { id: 'adapter-1', state: async () => ({}), waitUntilReady: async () => ({}) }
          }
        )
      : await createPublicBleManager(
          {
            supports: capabilities.supports,
            capability: capabilities.get,
            capabilities: capabilities.list,
            connect
          },
          () => clock.now,
          { peerId: value => opaqueId(value, 'peer', 'public-helper-test') }
        )
  return { manager, calls }
}

describe.each(['non-IPC', 'IPC'])('%s discovered connection public error boundary', host => {
  let clock
  beforeEach(() => {
    clock = { now: 1_000 }
    jest.spyOn(globalThis.performance, 'now').mockImplementation(() => clock.now)
  })
  afterEach(() => jest.restoreAllMocks())

  test('invalid options reject as BleError before acquisition', async () => {
    const { manager, calls } = await fixture(host, clock)
    const action = jest.fn()
    const pending = manager.withDiscoveredConnection('peer-1', { timeoutMs: 0 }, action)
    await expect(pending).rejects.toBeInstanceOf(BleError)
    await expect(pending).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(calls).toEqual({ connect: [], discover: [], release: 0 })
    expect(action).not.toHaveBeenCalled()
  })

  test('exhausted acquisition budget rejects as BleError, skips discovery and releases once', async () => {
    const { manager, calls } = await fixture(host, clock, {
      onConnect: () => {
        clock.now = 2_000
      }
    })
    const action = jest.fn()
    const pending = manager.withDiscoveredConnection('peer-1', { timeoutMs: 1_000 }, action)
    await expect(pending).rejects.toBeInstanceOf(BleError)
    await expect(pending).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(calls.connect[0].deadline).toBe(2_000)
    expect(calls.discover).toEqual([])
    expect(calls.release).toBe(1)
    expect(action).not.toHaveBeenCalled()
  })

  test('translates the helper timeout before aggregating failed cleanup', async () => {
    const { manager, calls } = await fixture(host, clock, {
      onConnect: () => {
        clock.now = 2_000
      },
      cleanup: failedCleanup
    })
    const action = jest.fn()
    const error = await manager.withDiscoveredConnection('peer-1', { timeoutMs: 1_000 }, action).catch(error => error)
    expect(error).toBeInstanceOf(AggregateError)
    expect(error.errors).toHaveLength(2)
    expect(error.errors[0]).toBeInstanceOf(BleError)
    expect(error.errors[0].code).toBe('operation.timed-out')
    expect(error.errors[1]).toBeInstanceOf(BleCleanupError)
    expect(calls.discover).toEqual([])
    expect(calls.release).toBe(1)
    expect(action).not.toHaveBeenCalled()
  })

  test('ordinary backend discovery failures still cross the public bridge', async () => {
    const { manager, calls } = await fixture(host, clock, {
      onDiscover: () => {
        throw contractError('operation.timed-out', 'gatt', 'helper-test.discover')
      }
    })
    const action = jest.fn()
    const pending = manager.withDiscoveredConnection('peer-1', { timeoutMs: 1_000 }, action)
    await expect(pending).rejects.toBeInstanceOf(BleError)
    await expect(pending).rejects.toMatchObject({ code: 'operation.timed-out', operation: 'helper-test.discover' })
    expect(calls.discover).toHaveLength(1)
    expect(calls.release).toBe(1)
    expect(action).not.toHaveBeenCalled()
  })

  test('translates the helper timeout before aggregating rejected cleanup', async () => {
    const cleanupError = new Error('release rejected')
    const { manager, calls } = await fixture(host, clock, {
      onConnect: () => {
        clock.now = 2_000
      },
      cleanupError
    })
    const action = jest.fn()
    const error = await manager.withDiscoveredConnection('peer-1', { timeoutMs: 1_000 }, action).catch(error => error)
    expect(error).toBeInstanceOf(AggregateError)
    expect(error.errors).toHaveLength(2)
    expect(error.errors[0]).toBeInstanceOf(BleError)
    expect(error.errors[0].code).toBe('operation.timed-out')
    expect(error.errors[1]).toBe(cleanupError)
    expect(calls.discover).toEqual([])
    expect(calls.release).toBe(1)
    expect(action).not.toHaveBeenCalled()
  })

  test.each([
    ['Error', () => new Error('application-owned')],
    ['BackendContractError', () => contractError('argument.invalid', 'core', 'application-owned')],
    ['non-Error object', () => ({ application: 'owned' })]
  ])('preserves an application-thrown %s by identity', async (_name, createError) => {
    const { manager, calls } = await fixture(host, clock)
    const error = createError()
    await expect(
      manager.withDiscoveredConnection('peer-1', {}, async () => {
        throw error
      })
    ).rejects.toBe(error)
    expect(calls.discover).toHaveLength(1)
    expect(calls.release).toBe(1)
  })

  test('retains the original user error inside a cleanup aggregate', async () => {
    const { manager, calls } = await fixture(host, clock, { cleanup: failedCleanup })
    const primary = contractError('argument.invalid', 'core', 'application-owned')
    const error = await manager
      .withDiscoveredConnection('peer-1', {}, async () => {
        throw primary
      })
      .catch(error => error)
    expect(error).toBeInstanceOf(AggregateError)
    expect(error.errors[0]).toBe(primary)
    expect(error.errors[1]).toBeInstanceOf(BleCleanupError)
    expect(calls.release).toBe(1)
  })

  test('connect and discovery retain the original acquisition deadline', async () => {
    const { manager, calls } = await fixture(host, clock, {
      onConnect: () => {
        clock.now = 1_400
      }
    })
    const action = jest.fn(async () => 'done')
    await expect(manager.withDiscoveredConnection('peer-1', { timeoutMs: 1_000 }, action)).resolves.toBe('done')
    expect(calls.connect[0].deadline).toBe(2_000)
    expect(calls.discover[0].deadline).toBe(2_000)
    expect(action).toHaveBeenCalledTimes(1)
    expect(calls.release).toBe(1)
  })
})
