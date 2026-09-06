const { WinRtSecurityBackend } = require('../../../src/backends/winrt/winrt-security')

function state(overrides = {}) {
  return {
    bond: 'not-bonded',
    encryption: 'unsupported',
    authentication: 'unsupported',
    secureConnections: 'unsupported',
    pairingPossible: true,
    ...overrides
  }
}

function options(overrides = {}) {
  return {
    signal: null,
    deadline: null,
    transport: 'auto',
    protection: 'system-default',
    ceremony: 'system',
    ...overrides
  }
}

function createBoundary() {
  const listeners = new Set()
  let pairResolve = null
  const pairCancellation = jest.fn(async () => {
    pairResolve?.({ outcome: 'cancelled', state: null, reason: null })
    return 'cancellation-requested'
  })
  const boundary = {
    securityState: jest.fn(peerId => {
      expect(peerId).toBe('peer-1')
      return { completion: Promise.resolve(state()), cancel: jest.fn(async () => 'already-terminal') }
    }),
    pair: jest.fn(() => {
      const completion = new Promise(resolve => {
        pairResolve = resolve
      })
      return {
        completion,
        cancel: pairCancellation
      }
    }),
    cancelPairing: jest.fn(() => ({ completion: Promise.resolve(), cancel: jest.fn(async () => 'already-terminal') })),
    unpair: jest.fn(() => ({
      completion: Promise.resolve('unpaired'),
      cancel: jest.fn(async () => 'already-terminal')
    })),
    onSecurityState: listener => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    emitSecurityState: record => {
      for (const listener of [...listeners]) listener(record)
    }
  }
  boundary.listeners = listeners
  boundary.pairCancellation = pairCancellation
  return boundary
}

describe('WinRT security backend adapter', () => {
  test('maps measured state, watches native state changes, and preserves unsupported measurements', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const stream = security.watch('peer-1')
    const iterator = stream[Symbol.asyncIterator]()

    await expect(security.state('peer-1', options())).resolves.toMatchObject({
      bond: 'not-bonded',
      encryption: 'unsupported',
      authentication: 'unsupported'
    })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 1, state: { bond: 'not-bonded' } } }
    })
    boundary.emitSecurityState({ nativePeerId: 'peer-1', state: state({ bond: 'bonded' }) })
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { sequence: 2, state: { bond: 'bonded' } } }
    })
    await iterator.return()
    await stream.close()
    security.close()
  })

  test('rejects a secureConnections generation it cannot select instead of ignoring it', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    for (const value of ['require', 'disallow']) {
      await expect(security.pair('peer-1', options({ secureConnections: value }))).rejects.toMatchObject({
        normalized: { code: 'capability.unsupported' }
      })
    }
    // Fail-closed means we never dispatched a native pairing for the values we
    // cannot honour.
    expect(boundary.pair).not.toHaveBeenCalled()
    // 'prefer' defers to the platform, so it proceeds to a native pairing.
    const pending = security.pair('peer-1', options({ secureConnections: 'prefer' }))
    expect(boundary.pair).toHaveBeenCalledTimes(1)
    await security.cancelPairing('peer-1', options())
    await expect(pending).resolves.toEqual({ outcome: 'cancelled' })
    security.close()
  })

  test('arbitrates pairing and maps cancellation to one terminal result', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    await expect(security.pair('peer-1', options({ protection: 'authenticated' }))).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    const first = security.pair('peer-1', options())
    await expect(security.pair('peer-1', options())).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
    await expect(security.cancelPairing('peer-1', options())).resolves.toEqual({ outcome: 'cancelled' })
    await expect(first).resolves.toEqual({ outcome: 'cancelled' })
    expect(security.activePairings.size).toBe(0)
    await expect(security.cancelPairing('peer-1', options())).resolves.toEqual({ outcome: 'not-pairing' })
    await expect(security.unpair('peer-1', options())).resolves.toEqual({ outcome: 'unpaired' })
    security.close()
  })

  test('honors a pre-aborted cancelPairing request before native cancellation dispatch', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const first = security.pair('peer-1', options())
    const controller = new AbortController()
    controller.abort()

    await expect(
      security.cancelPairing('peer-1', { signal: controller.signal, deadline: null })
    ).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(boundary.cancelPairing).not.toHaveBeenCalled()
    expect(boundary.pairCancellation).not.toHaveBeenCalled()
    expect(security.activePairings.size).toBe(1)
    await expect(security.cancelPairing('peer-1', options())).resolves.toEqual({ outcome: 'cancelled' })
    await expect(first).resolves.toEqual({ outcome: 'cancelled' })
    security.close()
  })

  test('times out a never-resolving pairing-cancel acknowledgement without inventing cancelled', async () => {
    jest.useFakeTimers()
    try {
      const boundary = createBoundary()
      boundary.pair = jest.fn(() => ({
        completion: new Promise(() => undefined),
        cancel: jest.fn(() => new Promise(() => undefined))
      }))
      const security = new WinRtSecurityBackend(boundary, () => 50)
      const pendingPair = security.pair('peer-1', options())
      pendingPair.catch(() => undefined)
      const pendingCancel = security.cancelPairing('peer-1', { signal: null, deadline: 60 })
      const seen = []
      pendingCancel.then(
        value => seen.push(value),
        error => seen.push(error)
      )

      await jest.advanceTimersByTimeAsync(10)
      expect(seen).toHaveLength(1)
      expect(seen[0]).toMatchObject({
        normalized: { code: 'operation.timed-out', operation: 'winrt.security.cancel-pairing' }
      })
      expect(boundary.cancelPairing).not.toHaveBeenCalled()
      expect(security.activePairings.size).toBe(1)
      security.close()
    } finally {
      jest.useRealTimers()
    }
  })

  test('times out after a cancel ack when the pairing result never arrives', async () => {
    jest.useFakeTimers()
    try {
      const boundary = createBoundary()
      boundary.pair = jest.fn(() => ({
        completion: new Promise(() => undefined),
        cancel: jest.fn(async () => 'cancellation-requested')
      }))
      const security = new WinRtSecurityBackend(boundary, () => 50)
      const pendingPair = security.pair('peer-1', options())
      pendingPair.catch(() => undefined)
      const pendingCancel = security.cancelPairing('peer-1', { signal: null, deadline: 60 })
      const seen = []
      pendingCancel.then(
        value => seen.push(value),
        error => seen.push(error)
      )

      await jest.advanceTimersByTimeAsync(10)
      expect(seen).toHaveLength(1)
      expect(seen[0]).toMatchObject({ normalized: { code: 'operation.timed-out' } })
      expect(boundary.cancelPairing).toHaveBeenCalledTimes(1)
      expect(security.activePairings.size).toBe(1)
      security.close()
    } finally {
      jest.useRealTimers()
    }
  })

  test('reports paired when the in-flight pairing wins and completes successfully', async () => {
    const boundary = createBoundary()
    let resolvePair = () => undefined
    boundary.pair = jest.fn(() => ({
      completion: new Promise(resolve => {
        resolvePair = resolve
      }),
      cancel: jest.fn(async () => 'cancellation-requested')
    }))
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const pendingPair = security.pair('peer-1', options())
    const pendingCancel = security.cancelPairing('peer-1', options())

    resolvePair({ outcome: 'paired', state: state({ bond: 'bonded' }), reason: null })
    await expect(pendingPair).resolves.toMatchObject({ outcome: 'paired' })
    await expect(pendingCancel).resolves.toEqual({ outcome: 'paired' })
    security.close()
  })

  test('gives concurrent cancel waiters independent deadlines without dropping pairing ownership', async () => {
    jest.useFakeTimers()
    try {
      const boundary = createBoundary()
      let resolvePair = () => undefined
      boundary.pair = jest.fn(() => ({
        completion: new Promise(resolve => {
          resolvePair = resolve
        }),
        cancel: jest.fn(async () => 'cancellation-requested')
      }))
      const security = new WinRtSecurityBackend(boundary, () => 50)
      const pendingPair = security.pair('peer-1', options())
      const shortWait = security.cancelPairing('peer-1', { signal: null, deadline: 60 })
      const longWait = security.cancelPairing('peer-1', { signal: null, deadline: 80 })
      const seen = []
      shortWait.then(
        value => seen.push(value),
        error => seen.push(error)
      )

      await jest.advanceTimersByTimeAsync(10)
      expect(seen).toHaveLength(1)
      expect(seen[0]).toMatchObject({ normalized: { code: 'operation.timed-out' } })
      expect(security.activePairings.size).toBe(1)

      resolvePair({ outcome: 'paired', state: state({ bond: 'bonded' }), reason: null })
      await expect(pendingPair).resolves.toMatchObject({ outcome: 'paired' })
      await expect(longWait).resolves.toEqual({ outcome: 'paired' })
      security.close()
    } finally {
      jest.useRealTimers()
    }
  })

  test('settles a pending pairing at its deadline and requests native cancellation', async () => {
    jest.useFakeTimers()
    try {
      const boundary = createBoundary()
      const security = new WinRtSecurityBackend(boundary, () => 50)
      const pairing = security.pair('peer-1', options({ deadline: 60 }))
      const result = expect(pairing).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })

      await jest.advanceTimersByTimeAsync(10)

      await result
      expect(boundary.pairCancellation).toHaveBeenCalledTimes(1)
      security.close()
    } finally {
      jest.useRealTimers()
    }
  })

  test('honors cancellation and deadlines for state and unpair operations before native dispatch', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const controller = new AbortController()
    controller.abort()

    await expect(security.state('peer-1', { signal: controller.signal, deadline: null })).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    await expect(security.unpair('peer-1', { signal: controller.signal, deadline: null })).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    expect(boundary.securityState).not.toHaveBeenCalled()
    expect(boundary.unpair).not.toHaveBeenCalled()
    security.close()
  })

  test('rejects malformed native security state payloads', async () => {
    const boundary = createBoundary()
    boundary.securityState = jest.fn(() => ({
      completion: Promise.resolve({ ...state(), pairingPossible: 'yes' }),
      cancel: jest.fn(async () => 'already-terminal')
    }))
    const security = new WinRtSecurityBackend(boundary, () => 50)

    await expect(security.state('peer-1', options())).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'winrt.security.state' }
    })
    security.close()
  })

  test('rejects pair results with unknown fields instead of projecting them', async () => {
    const boundary = createBoundary()
    boundary.pair = jest.fn(() => ({
      completion: Promise.resolve({ outcome: 'paired', state: state(), reason: null, unexpected: true }),
      cancel: jest.fn(async () => 'already-terminal')
    }))
    const security = new WinRtSecurityBackend(boundary, () => 50)

    await expect(security.pair('peer-1', options())).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'winrt.security.pair-result' }
    })
    security.close()
  })

  test('fails closed when a security state-change payload is malformed', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const stream = security.watch('peer-1')
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()

    expect(() => boundary.emitSecurityState({ nativePeerId: 'peer-1', state: { ...state(), bond: 'invalid' } })).not.toThrow()
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'source-failed' } })
    security.close()
  })

  test('terminates security watches during adapter loss and ignores retired listener callbacks', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const stream = security.watch('peer-1')
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const terminal = iterator.next()
    const oldListener = [...boundary.listeners][0]

    security.resetForAdapterLoss()
    await expect(terminal).resolves.toMatchObject({ value: { kind: 'terminal', reason: 'connection-lost' } })
    oldListener({ nativePeerId: 'peer-1', state: state({ bond: 'bonded' }) })
    expect(security.streams.size).toBe(0)

    security.adapterRecovered()
    const recovered = security.watch('peer-1')
    const recoveredIterator = recovered[Symbol.asyncIterator]()
    await recoveredIterator.next()
    boundary.emitSecurityState({ nativePeerId: 'peer-1', state: state({ bond: 'bonded' }) })
    await expect(recoveredIterator.next()).resolves.toMatchObject({
      value: { kind: 'value', value: { state: { bond: 'bonded' } } }
    })
    await recoveredIterator.return()
    await recovered.close()
    security.close()
  })

  test('releases failed security watches from backend ownership', async () => {
    const boundary = createBoundary()
    const failure = new Error('state failed')
    boundary.securityState = jest.fn(() => ({
      completion: Promise.reject(failure),
      cancel: jest.fn(async () => 'already-terminal')
    }))
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const stream = security.watch('peer-1')
    await new Promise(resolve => setImmediate(resolve))
    expect(security.streams.size).toBe(0)
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    security.close()
  })

  test('translates public WinRT peer IDs at the native security boundary', async () => {
    const boundary = createBoundary()
    boundary.securityState = jest.fn(nativePeerId => {
      expect(nativePeerId).toBe('C0FFEE000001')
      return { completion: Promise.resolve(state()), cancel: jest.fn(async () => 'already-terminal') }
    })
    const security = new WinRtSecurityBackend(
      boundary,
      () => 50,
      undefined,
      () => 'C0FFEE000001',
      nativePeerId => (nativePeerId === 'C0FFEE000001' ? 'public-peer-1' : null)
    )

    await expect(security.state('public-peer-1', options())).resolves.toMatchObject({ bond: 'not-bonded' })
    const stream = security.watch('public-peer-1')
    const iterator = stream[Symbol.asyncIterator]()
    await iterator.next()
    const next = iterator.next()
    boundary.emitSecurityState({ nativePeerId: 'C0FFEE000001', state: state({ bond: 'bonded' }) })
    await expect(next).resolves.toMatchObject({
      value: { value: { peerId: 'public-peer-1', state: { bond: 'bonded' } } }
    })
    await iterator.return()
    await stream.close()
    security.close()
  })
})
