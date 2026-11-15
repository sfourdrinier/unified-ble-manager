const { createContinuationRecordingController, createNativeContinuationRecordingController } = require('../../src/core/continuation-recording')

test('an unknown stop receipt offer is rejected before dispatch', async () => {
  const stopOrdered = jest.fn()
  const controller = createContinuationRecordingController({ orderedReceiptVersion: async () => 3, stopOrdered })
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'protocol.incompatible' })
  expect(stopOrdered).not.toHaveBeenCalled()
})

test('the negotiated v2 stop receipt is decoded exactly', async () => {
  const controller = createContinuationRecordingController({
    orderedReceiptVersion: async () => 2,
    stopOrdered: async () => ({ recording: { version: 2, state: 'finished', error: null }, hostCleanup: { state: 'pending' } })
  })
  expect(await controller.stopOrdered('sample')).toMatchObject({ recording: { version: 2, state: 'finished' } })
})

test('a native control with no version offer cannot dispatch stop', async () => {
  const stopOrdered = jest.fn()
  const controller = createNativeContinuationRecordingController({ stopOrdered }, 'native')
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'protocol.incompatible' })
  expect(stopOrdered).not.toHaveBeenCalled()
})

test('historical v1 map text remains readable when its UTF-8 size exceeds the v2 per-field limit', () => {
  const { parseOrderedRecordingFailure } = require('../../src/core/ordered-recording')
  const failure = { code: 'platform.failure', domain: 'stream', operation: 'notify', detail: null,
    platformDomain: null, platformCode: null, platformMessage: null,
    platformMetadata: { label: { kind: 'text', value: 'é'.repeat(32769) } } }
  expect(parseOrderedRecordingFailure(failure, 1).platformMetadata.label.value).toEqual(failure.platformMetadata.label.value)
  expect(() => parseOrderedRecordingFailure(failure, 2)).toThrow()
})
