const { claimNativeContinuationBacklog, claimNativeContinuationPrefixV1 } = require('../../src/core/native-continuation-claim')

const claim = (token, ordinal, more = false) => ({
  claimToken: token, consumerCount: 1,
  selectors: [{ serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', serviceOccurrence: 1, characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb', characteristicOccurrence: 1 }],
  batches: [JSON.stringify({ more, controlLost: 0, records: [{ t: 'value', ordinal, consumer: 'ubm-continuation-0', valueB64: 'AQ==', delivery: 'notification' }] })],
  disposed: false, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null
})
const receipt = disposed => ({ disposed, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null })

test('5.2.1 claim automatically acknowledges and reports cleanup', async () => {
  const access = { prepareClaim: jest.fn(async () => claim('old', 1)), acknowledgeClaim: jest.fn(async () => receipt(true)) }
  const result = await claimNativeContinuationBacklog(access, undefined, 'test')
  expect(access.acknowledgeClaim).toHaveBeenCalledWith('old')
  expect(result.disposed).toBe(true)
  expect(result.acknowledge).toBeUndefined()
})

test('5.2.1 claim preserves decoded data when ACK is uncertain', async () => {
  const access = { prepareClaim: async () => claim('old', 1), acknowledgeClaim: async () => { throw new Error('lost reply') } }
  const result = await claimNativeContinuationBacklog(access, undefined, 'test')
  expect(result.values).toHaveLength(1)
  expect(result.disposed).toBe(false)
  expect(result.disposeFailure).toContain('lost reply')
})

test('retained prefix requires a distinct versioned native operation before any preparation', async () => {
  const access = { prepareClaim: jest.fn(async () => claim('old', 1)), acknowledgeClaim: jest.fn() }
  await expect(claimNativeContinuationPrefixV1(access, undefined, 'test')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(access.prepareClaim).not.toHaveBeenCalled()
})

test('retained handoff retries lost ACK and preserves ordered tail', async () => {
  let attempts = 0
  const access = {
    prepareRetainedClaimV1: jest.fn().mockResolvedValueOnce(claim('a', 1, true)).mockResolvedValueOnce(claim('b', 2)),
    acknowledgeRetainedClaimV1: jest.fn(async token => {
      if (token === 'a' && attempts++ === 0) throw new Error('lost reply')
      return receipt(token === 'b')
    }),
    retireRetainedClaimReceiptV1: jest.fn(async () => ({}))
  }
  const first = await claimNativeContinuationPrefixV1(access, { maxItems: 1 }, 'test')
  expect(access.acknowledgeRetainedClaimV1).not.toHaveBeenCalled()
  await expect(first.acknowledge()).rejects.toBeDefined()
  expect(await claimNativeContinuationPrefixV1(access, undefined, 'test')).toBe(first)
  expect((await first.acknowledge()).disposed).toBe(false)
  const tail = await claimNativeContinuationPrefixV1(access, { maxItems: 1 }, 'test')
  expect([...first.records, ...tail.records].map(r => r.ordinal)).toEqual([1, 2])
  expect((await tail.acknowledge()).disposed).toBe(true)
})

test('abandoning a prepared prefix does not ACK; another claim returns the same owner', async () => {
  const access = { prepareRetainedClaimV1: jest.fn(async () => claim('abandoned', 1)), acknowledgeRetainedClaimV1: jest.fn(async () => receipt(true)), retireRetainedClaimReceiptV1: jest.fn(async () => ({})) }
  const first = await claimNativeContinuationPrefixV1(access, undefined, 'test')
  const resumed = await claimNativeContinuationPrefixV1(access, undefined, 'test')
  expect(resumed).toBe(first)
  expect(access.acknowledgeRetainedClaimV1).not.toHaveBeenCalled()
  expect((await resumed.acknowledge()).disposed).toBe(true)
  expect(access.prepareRetainedClaimV1).toHaveBeenCalledTimes(1)
})

test('legacy claim exposes all batches before auto ACK, with final cleanup outcome', async () => {
  const first = claim('complete', 1)
  const second = claim('complete', 2)
  const access = {
    prepareClaim: async () => ({ ...first, batches: [...first.batches, ...second.batches] }),
    acknowledgeClaim: jest.fn(async () => ({ disposed: false, afterCutoffLoss: { items: 2, bytes: 3 }, disposeFailure: 'cleanup failed' }))
  }
  const result = await claimNativeContinuationBacklog(access, { maxItems: 1 }, 'test')
  expect(result.values).toHaveLength(2)
  expect(result.afterCutoffLoss).toEqual({ items: 2, bytes: 3 })
  expect(result.disposeFailure).toBe('cleanup failed')
  expect(access.acknowledgeClaim).toHaveBeenCalledTimes(1)
})

test('an abandoned pending prepare stays owned and a later caller can accept the same prefix', async () => {
  let finish
  const prepare = new Promise(resolve => { finish = resolve })
  const access = { prepareRetainedClaimV1: jest.fn(() => prepare), acknowledgeRetainedClaimV1: jest.fn(async () => receipt(true)), retireRetainedClaimReceiptV1: jest.fn(async () => ({})) }
  const abandoned = claimNativeContinuationPrefixV1(access, undefined, 'test')
  const resumed = claimNativeContinuationPrefixV1(access, undefined, 'test')
  finish(claim('pending', 1))
  expect(await resumed).toBe(await abandoned)
  expect(access.prepareRetainedClaimV1).toHaveBeenCalledTimes(1)
  expect(access.acknowledgeRetainedClaimV1).not.toHaveBeenCalled()
  await (await resumed).acknowledge()
})

test('invalid retained records fail before acknowledgement', async () => {
  const invalid = claim('bad', 1)
  invalid.batches = ['not-json']
  const access = { prepareRetainedClaimV1: async () => invalid, acknowledgeRetainedClaimV1: jest.fn(), retireRetainedClaimReceiptV1: jest.fn(async () => ({})) }
  await expect(claimNativeContinuationPrefixV1(access, undefined, 'test')).rejects.toBeDefined()
  expect(access.acknowledgeRetainedClaimV1).not.toHaveBeenCalled()
})
