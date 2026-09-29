const { BleError } = require('../src/public/errors')
const { BackendContractError, contractError } = require('../src/backend-contract/errors')
const {
  encodeNativeContinuationFailure,
  decodeNativeContinuationEnvelope
} = require('../src/core/native-continuation-envelope')
const { createNativeContinuationControl } = require('../src/backends/desktop/native-continuation-controller')
const { createProcessDispatch } = require('../example-electron/driver/process-dispatch.cjs')
const { createProcessControls } = require('../example-electron/driver/process-controls.cjs')
const { createProcessSession } = require('../example-electron/driver/process-session.cjs')

const platform = {
  domain: 'sqlite',
  code: 'storage.io',
  safeMessage: 'recording directory cannot be resolved',
  metadata: { storageKind: 'storage.io', operation: 'configure', sqliteExtendedCode: 14 }
}
const failure = () =>
  new BleError('platform.failure', 'platform', 'continuation.recording', { platform, retryability: 'caller-decides' })

test.each(['configuration', 'offline store opening'])(
  '%s preserves native failure across the actual application dispatch',
  async kind => {
    const original = failure()
    const execute = jest.fn()
    const recordings = jest.fn(async () => {
      throw original
    })
    const createHost = jest.fn(async () => ({ continuation: { recordings }, continuationAccess: { execute } }))
    const session = createProcessSession({ createProcessHost: createHost, openRecordings: recordings })
    const controls = createProcessControls(session, '/trusted/app/recordings', async () => {})
    const dispatch = createProcessDispatch({
      controls: async () => controls,
      recordings: () => session.recordings()
    })
    const request =
      kind === 'configuration'
        ? { operation: 'execute', args: { peerId: 'peer', declarationJson: '{"recording":{"id":"journal"}}' } }
        : { operation: 'recording-status', args: { id: 'journal' } }
    const response = await dispatch(request)
    expect(recordings).toHaveBeenCalledTimes(1)
    expect(createHost).toHaveBeenCalledTimes(kind === 'configuration' ? 1 : 0)
    expect(execute).not.toHaveBeenCalled()
    expect(JSON.parse(response)).toMatchObject({ ok: false, commit: null, retryability: 'caller-decides' })
    expect(() => decodeNativeContinuationEnvelope(response, 'electron-reference')).toThrow(BackendContractError)
    try {
      decodeNativeContinuationEnvelope(response, 'electron-reference')
    } catch (error) {
      expect(error.normalized).toMatchObject({
        code: original.code,
        domain: original.domain,
        operation: original.operation,
        platform,
        retryability: 'caller-decides'
      })
    }
  }
)

test('renderer factory receives the original sqlite identity rather than native-bridge transport failure', async () => {
  const dispatch = createProcessDispatch({
    controls: async () => ({
      execute: async () => {
        throw failure()
      }
    }),
    recordings: async () => {
      throw new Error('unused')
    }
  })
  const control = createNativeContinuationControl({
    execute: (peerId, declarationJson) => dispatch({ operation: 'execute', args: { peerId, declarationJson } })
  })
  await expect(control.execute({ onAppearance: 'native', peerId: 'peer', resubscribe: [] })).rejects.toMatchObject({
    code: 'platform.failure',
    operation: 'continuation.recording',
    platform
  })
})

test('canonical helper round-trips backend errors without replacing native metadata', () => {
  const original = contractError('operation.timed-out', 'gatt', 'gatt.read', platform)
  expect(() =>
    decodeNativeContinuationEnvelope(encodeNativeContinuationFailure(original), 'electron-reference')
  ).toThrow(BackendContractError)
  try {
    decodeNativeContinuationEnvelope(encodeNativeContinuationFailure(original), 'electron-reference')
  } catch (error) {
    expect(error.normalized).toEqual(original.normalized)
  }
})

test('unknown errors and lookalikes remain errors; validation happens before acquisition', async () => {
  for (const error of [new Error('unknown'), { code: 'platform.failure', domain: 'platform', operation: 'forged' }]) {
    expect(() => encodeNativeContinuationFailure(error)).toThrow()
    const dispatch = createProcessDispatch({
      controls: async () => {
        throw error
      },
      recordings: async () => {
        throw error
      }
    })
    await expect(dispatch({ operation: 'status', args: {} })).rejects.toBe(error)
  }
  const acquire = jest.fn()
  const dispatch = createProcessDispatch({ controls: acquire, recordings: acquire })
  await expect(dispatch({ operation: 'recording-clear', args: { id: 'a', directory: '/private' } })).rejects.toThrow(
    'arguments refused'
  )
  expect(acquire).not.toHaveBeenCalled()
})

test('unsupported native error fields are refused rather than silently dropped', () => {
  const nested = new BleError('platform.failure', 'platform', 'native', {
    platform: { ...platform, metadata: { nested: [] } }
  })
  expect(() => encodeNativeContinuationFailure(nested)).toThrow(BackendContractError)
  const committed = new BleError('operation.timed-out', 'gatt', 'write', { commit: 'uncertain', retryability: 'never' })
  expect(() => encodeNativeContinuationFailure(committed)).toThrow(BackendContractError)
})
