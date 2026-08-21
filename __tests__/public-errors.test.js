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
    const platform = {
      domain: 'test-host',
      code: 'E_TEST',
      safeMessage: 'safe test detail',
      metadata: { nested: { value: 'original' } }
    }
    const error = new BleError('operation.timed-out', 'core', 'public-errors.test', { platform })

    expect(error.platform).not.toBe(platform)
    expect(error.platform).toEqual(platform)
    expect(Object.isFrozen(error.platform)).toBe(true)
    expect(Object.isFrozen(error.platform.metadata)).toBe(true)
    expect(Object.isFrozen(error.platform.metadata.nested)).toBe(true)
    expect(Object.isFrozen(error.recovery.actions[0])).toBe(true)
  })

  test('preserves unexpected errors at the public boundary', () => {
    const unexpected = new Error('unexpected')

    expect(rehydratePublicError(unexpected)).toBe(unexpected)
  })
})
