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

  test('arbitrates pairing and maps cancellation to one terminal result', async () => {
    const boundary = createBoundary()
    const security = new WinRtSecurityBackend(boundary, () => 50)
    const first = security.pair('peer-1', options())
    await expect(security.pair('peer-1', options())).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
    await expect(security.cancelPairing('peer-1', options())).resolves.toEqual({ outcome: 'cancelled' })
    await expect(first).resolves.toEqual({ outcome: 'cancelled' })
    await expect(security.cancelPairing('peer-1', options())).resolves.toEqual({ outcome: 'not-pairing' })
    await expect(security.unpair('peer-1', options())).resolves.toEqual({ outcome: 'unpaired' })
    security.close()
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
})
