const { claimNativeContinuationPrefixV1 } = require('../../src/core/native-continuation-claim')

const prepared = { claimToken: 'token', consumerCount: 0, selectors: [], batches: [], disposed: false, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null }
const receipt = { disposed: true, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null }

test('V1 admission requires receipt retirement before preparing', async () => {
  const access = { prepareRetainedClaimV1: jest.fn(), acknowledgeRetainedClaimV1: jest.fn() }
  await expect(claimNativeContinuationPrefixV1(access, undefined, 'test')).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(access.prepareRetainedClaimV1).not.toHaveBeenCalled()
})

test('retirement is explicit after acknowledgement and retries the exact token', async () => {
  let attempts = 0
  const access = {
    prepareRetainedClaimV1: async () => prepared,
    acknowledgeRetainedClaimV1: jest.fn(async () => receipt),
    retireRetainedClaimReceiptV1: jest.fn(async () => { if (++attempts === 1) throw Error('lost reply'); return {} })
  }
  const prefix = await claimNativeContinuationPrefixV1(access, undefined, 'test')
  await expect(prefix.retireReceipt()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  await prefix.acknowledge()
  await expect(prefix.retireReceipt()).rejects.toThrow('lost reply')
  await prefix.retireReceipt()
  await prefix.retireReceipt()
  expect(access.retireRetainedClaimReceiptV1.mock.calls).toEqual([['token'], ['token']])
  expect(await prefix.acknowledge()).toEqual(receipt)
})

test('legacy handoff keeps the 5.2.1 result shape for an ordinary value', async () => {
  const legacy = {
    claimToken: 'legacy', consumerCount: 1,
    selectors: [{ serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', serviceOccurrence: 1, characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb', characteristicOccurrence: 1 }],
    batches: [JSON.stringify({ records: [{ t: 'value', ordinal: 1, consumer: 'ubm-continuation-0', valueB64: 'AQ==', delivery: 'notification' }], more: false, controlLost: 0 })],
    disposed: false, afterCutoffLoss: { items: 0, bytes: 0 }, disposeFailure: null
  }
  const { claimNativeContinuationBacklog } = require('../../src/core/native-continuation-claim')
  const result = await claimNativeContinuationBacklog({ prepareClaim: async () => legacy, acknowledgeClaim: async () => receipt }, undefined, 'test')
  expect(Object.hasOwn(result, 'records')).toBe(false)
  expect(result.values[0]).toEqual({ consumer: 'ubm-continuation-0', value: new Uint8Array([1]), delivery: 'notification' })
})
