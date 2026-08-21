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
  })
})
