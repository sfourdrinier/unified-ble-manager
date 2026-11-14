const { recordingDatabase } = require('./web-recovery-522-helper')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')

test('committing a prepared attempt after its incarnation disappeared fences it as not dispatched', async () => {
  const database = recordingDatabase()
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({ database, locks })
  const attempt = { version: 1, recordingId: 'sample', producerIncarnation: 'old', attemptId: 'b'.repeat(64) }
  const recovery = createWebRecordingRecovery(database)
  expect(await recovery.getClearAttemptV1(attempt)).toEqual({ outcome: 'unknown' })
  await expect(controller.commitClearAttemptV1(attempt)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(await recovery.getClearAttemptV1(attempt)).toEqual({ outcome: 'superseded', attempt })
  await expect(controller.commitClearAttemptV1(attempt)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(await recovery.getClearAttemptV1(attempt)).toEqual({ outcome: 'superseded', attempt })
})
