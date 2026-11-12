const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')

const attempt = {
  version: 1,
  recordingId: 'sample',
  producerIncarnation: 'producer',
  attemptId: 'a'.repeat(64)
}

afterEach(() => jest.useRealTimers())

test('unsettled recovery readiness returns unknown, caps inflight reads, and never dispatches after cancellation', async () => {
  jest.useFakeTimers()
  const releases = []
  let dispatched = 0
  const database = {
    ready: () => new Promise(resolve => { releases.push(resolve) }),
    transaction: () => { dispatched++; throw new Error('must not dispatch') }
  }
  const first = createWebRecordingRecovery(database)
  const second = createWebRecordingRecovery(database)
  const abort = new AbortController()
  const one = first.getClearAttemptV1(attempt, { signal: abort.signal })
  const two = second.getClearAttemptV1(attempt)
  abort.abort()
  expect(await one).toEqual({ outcome: 'unknown' })
  expect(await second.getClearAttemptV1(attempt)).toEqual({ outcome: 'unknown' })
  await jest.advanceTimersByTimeAsync(5000)
  expect(await two).toEqual({ outcome: 'unknown' })
  for (const release of releases) release()
  await Promise.resolve()
  await Promise.resolve()
  expect(dispatched).toBe(0)
  jest.useRealTimers()
})

test('a cancelled dispatched recovery read requests transaction abort and reports unknown', async () => {
  let aborted = 0
  let dispatched
  const started = new Promise(resolve => { dispatched = resolve })
  const database = {
    transaction() {
      dispatched()
      const tx = {
        onabort: null,
        oncomplete: null,
        error: null,
        objectStore: () => ({ get: () => ({ onsuccess: null }) }),
        abort() { aborted++; tx.onabort?.() }
      }
      return tx
    }
  }
  const controller = new AbortController()
  const pending = createWebRecordingRecovery(database).getClearAttemptV1(attempt, { signal: controller.signal })
  await started
  controller.abort()
  expect(await pending).toEqual({ outcome: 'unknown' })
  expect(aborted).toBe(1)
})

test('separate connections to the same named journal share the pending-read bound', async () => {
  const releases = []
  const database = () => ({
    name: 'same-journal',
    ready: () => new Promise(resolve => releases.push(resolve)),
    transaction: jest.fn()
  })
  const first = database(), second = database(), third = database()
  const a = new AbortController(), b = new AbortController(), c = new AbortController()
  const pendingA = createWebRecordingRecovery(first).getClearAttemptV1(attempt, { signal: a.signal })
  const pendingB = createWebRecordingRecovery(second).getClearAttemptV1(attempt, { signal: b.signal })
  const pendingC = createWebRecordingRecovery(third).getClearAttemptV1(attempt, { signal: c.signal })
  const count = releases.length
  a.abort(); b.abort(); c.abort()
  expect(await Promise.all([pendingA, pendingB, pendingC])).toEqual([{ outcome: 'unknown' }, { outcome: 'unknown' }, { outcome: 'unknown' }])
  for (const release of releases) release()
  expect(count).toBe(2)
})
