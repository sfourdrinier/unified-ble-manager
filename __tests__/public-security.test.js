const { createPublicBleManager } = require('../src/public/ble-manager')
const { withRequiredSecurity } = require('../src/public/security')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { capacity } = require('../src/backend-contract/primitives')
const { contractError } = require('../src/backend-contract/errors')

function measuredState(overrides = {}) {
  return Object.freeze({
    bond: 'not-bonded',
    encryption: 'not-encrypted',
    authentication: 'unauthenticated',
    secureConnections: 'no',
    pairingPossible: true,
    measuredAtMonotonicMs: 100,
    limitations: Object.freeze([]),
    ...overrides
  })
}

function securityStream() {
  return new CoreBoundedStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(1024), reservedControlCapacity: capacity(1) },
    'error'
  )
}

function internalWithSecurity(security) {
  return {
    supports: id =>
      id === 'security:state' ||
      id === 'security:pair' ||
      id === 'security:cancel-pairing' ||
      id === 'security:unpair' ||
      id === 'security:custom-ceremony',
    capability: id =>
      id === 'security:state' ||
      id === 'security:pair' ||
      id === 'security:cancel-pairing' ||
      id === 'security:unpair' ||
      id === 'security:custom-ceremony'
        ? { id, state: 'supported' }
        : null,
    capabilities: () => [],
    attachedBackend: { backend: { security } },
    destroy: jest.fn(),
    scan: jest.fn(),
    connect: jest.fn()
  }
}

describe('public security façade', () => {
  test('normalizes peer and operation options for state, pair, cancel, and unpair', async () => {
    const stream = securityStream()
    const security = {
      state: jest.fn(async (peerId, options) => {
        expect(peerId).toBe('peer-1')
        expect(options.deadline).toBe(125)
        return measuredState()
      }),
      watch: jest.fn(() => stream),
      pair: jest.fn(async (peerId, options) => {
        expect(peerId).toBe('peer-1')
        expect(options.transport).toBe('auto')
        expect(options.protection).toBe('authenticated')
        expect(options.ceremony).toBe('system')
        expect(options.deadline).toBe(150)
        return { outcome: 'paired', state: measuredState({ bond: 'bonded' }) }
      }),
      cancelPairing: jest.fn(async (peerId, options) => {
        expect(peerId).toBe('peer-1')
        expect(options.deadline).toBe(175)
        return { outcome: 'cancelled' }
      }),
      unpair: jest.fn(async (peerId, options) => {
        expect(peerId).toBe('peer-1')
        expect(options.deadline).toBe(200)
        return { outcome: 'unpaired' }
      })
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const peer = { id: 'peer-1', name: null, rssi: null }

    await expect(manager.security.state(peer, { timeoutMs: 25 })).resolves.toEqual(measuredState())
    await expect(
      manager.security.pair(peer, {
        timeoutMs: 50,
        transport: 'auto',
        protection: 'authenticated',
        ceremony: 'system'
      })
    ).resolves.toMatchObject({ outcome: 'paired', state: { bond: 'bonded' } })
    await expect(manager.security.cancelPairing(peer, { timeoutMs: 75 })).resolves.toEqual({
      outcome: 'cancelled'
    })
    await expect(manager.security.unpair(peer, { timeoutMs: 100 })).resolves.toEqual({ outcome: 'unpaired' })

    expect(security.state).toHaveBeenCalledTimes(1)
    expect(security.pair).toHaveBeenCalledTimes(1)
    expect(security.cancelPairing).toHaveBeenCalledTimes(1)
    expect(security.unpair).toHaveBeenCalledTimes(1)
  })

  test('maps backend security watch values and closes the backend iterator', async () => {
    const stream = securityStream()
    const security = {
      state: jest.fn(async () => measuredState()),
      watch: jest.fn(() => stream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const events = manager.security.watch({ id: 'peer-1', name: null, rssi: null })
    const iterator = events[Symbol.asyncIterator]()
    const next = iterator.next()
    const event = {
      kind: 'state',
      peerId: 'peer-1',
      sequence: 1,
      state: measuredState({ bond: 'bonded' })
    }
    stream.emit(event, 1)

    await expect(next).resolves.toEqual({ done: false, value: event })
    await expect(iterator.return()).resolves.toEqual({ done: true, value: undefined })
    expect(await stream[Symbol.asyncIterator]().return()).toEqual({ done: true, value: undefined })
  })

  test('P1-08 rejects a security watch event for the wrong peer or a non-monotonic sequence', async () => {
    const stream = securityStream()
    const security = {
      state: jest.fn(async () => measuredState()),
      watch: jest.fn(() => stream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const events = manager.security.watch({ id: 'peer-1', name: null, rssi: null })
    const iterator = events[Symbol.asyncIterator]()
    const next = iterator.next()
    stream.emit(
      {
        kind: 'state',
        peerId: 'peer-other',
        sequence: 1,
        state: measuredState({ bond: 'bonded' })
      },
      1
    )
    await expect(next).rejects.toMatchObject({ code: 'protocol.violation' })
  })

  test('rehydrates deferred security watch source and iterator failures', async () => {
    const backendError = contractError('protocol.violation', 'platform', 'public-security.watch-source')
    const sourceFailureSecurity = {
      state: jest.fn(),
      watch: jest.fn(() => Promise.reject(backendError)),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const sourceFailureManager = await createPublicBleManager(
      internalWithSecurity(sourceFailureSecurity),
      () => 100
    )
    const sourceFailureIterator = sourceFailureManager.security
      .watch({ id: 'peer-1', name: null, rssi: null })
      [Symbol.asyncIterator]()
    await expect(sourceFailureIterator.next()).rejects.toMatchObject({ code: 'protocol.violation' })

    const iteratorFailureStream = {
      [Symbol.asyncIterator]: () => ({
        next: async () => {
          throw backendError
        },
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      close: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const iteratorFailureSecurity = {
      state: jest.fn(),
      watch: jest.fn(() => iteratorFailureStream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const iteratorFailureManager = await createPublicBleManager(
      internalWithSecurity(iteratorFailureSecurity),
      () => 100
    )
    const iteratorFailure = iteratorFailureManager.security.watch({ id: 'peer-1', name: null, rssi: null })
    await expect(iteratorFailure[Symbol.asyncIterator]().next()).rejects.toMatchObject({ code: 'protocol.violation' })
    expect(iteratorFailureStream.close).toHaveBeenCalledTimes(1)
  })

  test('closes the backend security stream when its iterator reaches done', async () => {
    const stream = {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: true, value: undefined }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      close: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const security = {
      state: jest.fn(),
      watch: jest.fn(() => stream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const iterator = manager.security.watch({ id: 'peer-1', name: null, rssi: null })[Symbol.asyncIterator]()

    await expect(iterator.next()).resolves.toEqual({ done: true, value: undefined })
    expect(stream.close).toHaveBeenCalledTimes(1)
  })

  test('preserves overflow and cleanup failures together at the public watch boundary', async () => {
    const overflow = {
      kind: 'overflow',
      policy: 'error',
      droppedItems: 1,
      droppedBytes: 1,
      replacedItems: 0
    }
    const cleanupError = contractError('lifecycle.invalid-state', 'core', 'public-security.watch-overflow-close')
    const stream = {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: false, value: overflow }),
        return: async () => ({ done: true, value: undefined }),
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      close: jest.fn(async () => {
        throw cleanupError
      })
    }
    const security = {
      state: jest.fn(),
      watch: jest.fn(() => stream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const iterator = manager.security.watch({ id: 'peer-1', name: null, rssi: null })[Symbol.asyncIterator]()

    await expect(iterator.next()).rejects.toBeInstanceOf(AggregateError)
    expect(stream.close).toHaveBeenCalledTimes(1)
  })

  test('rehydrates and preserves both security watch return and close failures', async () => {
    const iteratorError = contractError('operation.aborted', 'core', 'public-security.watch-return')
    const closeError = contractError('lifecycle.invalid-state', 'core', 'public-security.watch-close')
    const stream = {
      [Symbol.asyncIterator]: () => ({
        next: async () => ({ done: false, value: { kind: 'terminal', reason: 'closed' } }),
        return: async () => {
          throw iteratorError
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }),
      close: jest.fn(async () => {
        throw closeError
      })
    }
    const security = {
      state: jest.fn(),
      watch: jest.fn(() => stream),
      pair: jest.fn(),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const iterator = manager.security.watch({ id: 'peer-1', name: null, rssi: null })[Symbol.asyncIterator]()

    await expect(iterator.next()).rejects.toBeInstanceOf(AggregateError)
    expect(stream.close).toHaveBeenCalledTimes(1)
  })

  test('adapts custom ceremony challenges without passing native objects or losing passkey zeros', async () => {
    const stream = securityStream()
    const agent = {
      onChallenge: jest.fn(async challenge => {
        expect(challenge).toMatchObject({
          kind: 'provide-passkey',
          peer: { id: 'peer-1' },
          challengeId: 'challenge-1',
          deadlineMonotonicMs: 500
        })
        expect(challenge.passkey).toBeUndefined()
        return { kind: 'provide-passkey', passkey: '012345' }
      })
    }
    const security = {
      state: jest.fn(async () => measuredState()),
      watch: jest.fn(() => stream),
      pair: jest.fn(async (_peerId, options) => {
        const response = await options.ceremony.agent.onChallenge({
          kind: 'provide-passkey',
          peerId: 'peer-1',
          challengeId: 'challenge-1',
          deadlineMonotonicMs: 500
        })
        expect(response).toEqual({ kind: 'provide-passkey', passkey: '012345' })
        return { outcome: 'rejected', reason: 'test-only' }
      }),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)

    await expect(manager.security.pair({ id: 'peer-1', name: null, rssi: null }, { ceremony: agent })).resolves.toEqual(
      { outcome: 'rejected', reason: 'test-only' }
    )
    expect(agent.onChallenge).toHaveBeenCalledTimes(1)
  })

  test('reports explicit unsupported state and rejects unsupported operations', async () => {
    const internal = {
      supports: () => false,
      capability: () => null,
      capabilities: () => [],
      attachedBackend: { backend: {} },
      destroy: jest.fn(),
      scan: jest.fn(),
      connect: jest.fn()
    }
    const manager = await createPublicBleManager(internal, () => 100)
    const peer = { id: 'peer-1', name: null, rssi: null }

    await expect(manager.security.state(peer)).resolves.toMatchObject({
      bond: 'unsupported',
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: null,
      measuredAtMonotonicMs: 100
    })
    await expect(manager.security.pair(peer)).rejects.toMatchObject({ code: 'capability.unsupported' })
    await expect(manager.security.cancelPairing(peer)).rejects.toMatchObject({ code: 'capability.unsupported' })
    await expect(manager.security.unpair(peer)).rejects.toMatchObject({ code: 'capability.unsupported' })
  })

  test('protected-GATT helper requires explicit pairing and invokes the action once after verification', async () => {
    const peer = { id: 'peer-1', name: null, rssi: null }
    const security = {
      state: jest.fn(async () => measuredState()),
      watch: jest.fn(() => securityStream()),
      pair: jest.fn(async () => ({
        outcome: 'paired',
        state: measuredState({ bond: 'bonded', encryption: 'encrypted' })
      })),
      cancelPairing: jest.fn(),
      unpair: jest.fn()
    }
    const manager = await createPublicBleManager(internalWithSecurity(security), () => 100)
    const action = jest.fn(async () => 'written')

    await expect(withRequiredSecurity(manager.security, peer, 'encrypted', action)).rejects.toMatchObject({
      code: 'platform.security',
      recovery: { actions: [{ kind: 'pair' }, { kind: 'repair' }] }
    })
    expect(action).not.toHaveBeenCalled()
    await expect(
      withRequiredSecurity(manager.security, peer, 'encrypted', action, { pair: { protection: 'encrypted' } })
    ).resolves.toBe('written')
    expect(action).toHaveBeenCalledTimes(1)
    expect(security.pair).toHaveBeenCalledTimes(1)
  })
})
