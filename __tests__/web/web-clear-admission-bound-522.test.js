const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { admissionLocks } = require('./web-clear-admission-locks-helper')
const flush = async () => {
  for (let i = 0; i < 100; i++) await Promise.resolve()
}

test('controllers retain admission slots until uncertain transactions settle, then permit a retry', async () => {
  jest.useFakeTimers()
  const transactions = []
  try {
    const database = {
      name: 'bounded-admission-test',
      transaction() {
        let used = false
        const tx = {
          error: new Error('aborted'),
          onabort: null,
          oncomplete: null,
          abort() {},
          objectStore() {
            used = true
            return {
              get() {
                return { onsuccess: null }
              }
            }
          },
          get used() {
            return used
          }
        }
        transactions.push(tx)
        queueMicrotask(() => {
          if (!used) tx.oncomplete?.()
        })
        return tx
      }
    }
    const locks = admissionLocks()
    const first = await createWebContinuationRecordingController({ database, locks })
    const second = await createWebContinuationRecordingController({ database: { ...database }, locks })
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }
    const attempts = []
    for (const controller of [first, second]) {
      attempts.push(controller.admitClearAttemptV2(input).catch(error => error))
      await flush()
      await jest.advanceTimersByTimeAsync(0)
      await flush()
    }
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    await jest.advanceTimersByTimeAsync(5000)
    for (const result of await Promise.all(attempts)) expect(result.code).toBe('capability.unavailable')
    await expect(first.admitClearAttemptV2(input)).rejects.toMatchObject({ code: 'capability.unavailable' })
    transactions.filter(tx => tx.used)[0].onabort()
    await flush()
    const retry = first.admitClearAttemptV2(input).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(0)
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(3)
    transactions
      .filter(tx => tx.used)
      .slice(1)
      .forEach(tx => tx.onabort())
    await retry
  } finally {
    jest.useRealTimers()
  }
})
