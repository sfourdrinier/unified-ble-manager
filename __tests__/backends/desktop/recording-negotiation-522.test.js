const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')
const { createNativeContinuationController } = require('../../../src/backends/desktop/native-continuation-controller')
const envelope = value => JSON.stringify({ ok: true, value })
const result = { recording: { version: 2, state: 'finished', error: null }, hostCleanup: { state: 'uncertain' } }

test.each([undefined, 0, 1, 3])('offline desktop stop refuses receipt offer %s before stop', async version => {
  const stopOrdered = jest.fn(async () => envelope(result))
  const binding = { openRecordingStore: async () => ({ stopOrdered }) }
  if (version !== undefined) binding.orderedRecordingReceiptVersion = () => version
  const controller = await openNativeContinuationRecordings(binding, '/private/app')
  await expect(controller.stopOrdered('recording')).rejects.toMatchObject({ code: 'protocol.incompatible' })
  expect(stopOrdered).not.toHaveBeenCalled()
})

test('desktop v2 stop preserves exact uncertain cleanup and malformed post-dispatch error', async () => {
  const stopOrdered = jest.fn(async () => envelope(result))
  const binding = { orderedRecordingReceiptVersion: () => 2, openRecordingStore: async () => ({ stopOrdered }) }
  const controller = await openNativeContinuationRecordings(binding, '/private/app')
  expect(await controller.stopOrdered('recording')).toEqual(result)
  stopOrdered.mockResolvedValueOnce(envelope({ ...result, recording: { ...result.recording, version: 1 } }))
  await expect(controller.stopOrdered('recording')).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(stopOrdered).toHaveBeenCalledTimes(2)
})

test('same-host stop negotiates before dispatch', async () => {
  const stopOrdered = jest.fn(async () => envelope(result))
  const central = { continuationConfigureRecordingDirectory: async () => envelope({ state: 'configured', encrypted: false }), continuationRecordingStore: () => ({ stopOrdered }), orderedRecordingReceiptVersion: () => 1 }
  const controller = await createNativeContinuationController(central).recordings('/private/app')
  await expect(controller.stopOrdered('recording')).rejects.toMatchObject({ code: 'protocol.incompatible' })
  expect(stopOrdered).not.toHaveBeenCalled()
})

test('v2 stop retains duplicate metadata and independent cleanup causes', async () => {
  const failure = {
    code: 'platform.failure', domain: 'gatt', operation: 'source.fence', detail: null,
    platformDomain: 'native', platformCode: 'failed', platformMessage: null, platformMetadata: {},
    platformMetadataEntries: [
      { key: 'duplicate', value: { kind: 'int', value: '-9223372036854775808' } },
      { key: 'duplicate', value: { kind: 'text', value: 'second' } }
    ],
    platformCleanup: [{ domain: 'cleanup', code: 'refused', message: null, metadata: {} }]
  }
  const receipt = { recording: { version: 2, state: 'failed', error: { kind: 'source', source: failure } }, hostCleanup: { state: 'uncertain' } }
  const binding = { orderedRecordingReceiptVersion: () => 2, openRecordingStore: async () => ({ stopOrdered: async () => envelope(receipt) }) }
  const controller = await openNativeContinuationRecordings(binding, '/private/app')
  expect(await controller.stopOrdered('recording')).toEqual(receipt)
})
