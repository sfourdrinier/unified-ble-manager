const { admissionLocks } = require('./web-clear-admission-locks-helper')
const flush = async () => {
  for (let i = 0; i < 100; i++) await Promise.resolve()
}

function stalledDatabase(name, queueThrows = false) {
  const transactions = []
  const database = {
    name,
    transaction() {
      let used = false
      const tx = {
        error: new Error('aborted'),
        onabort: null,
        oncomplete: null,
        abort() {},
        objectStore() {
          used = true
          if (queueThrows && transactions.filter(value => value.used).length === 1) throw new Error('queue failed')
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
  return { database, transactions }
}

test('the default admission bound applies across independent module realms', async () => {
  jest.useFakeTimers()
  const { database, transactions } = stalledDatabase('cross-realm-admission-test')
  try {
    const locks = admissionLocks()
    const constructors = []
    for (let i = 0; i < 3; i++)
      jest.isolateModules(() => {
        constructors.push(
          require('../../src/web/web-continuation-recording-controller').createWebContinuationRecordingController
        )
      })
    const controllers = await Promise.all(constructors.map(create => create({ database, locks })))
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }
    const attempts = []
    for (let i = 0; i < 3; i++) {
      attempts.push(
        controllers[i].admitClearAttemptV2({ ...input, requestId: String(i + 1).repeat(64) }).catch(error => error)
      )
      await flush()
      await jest.advanceTimersByTimeAsync(0)
      await flush()
    }
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    expect(transactions).toHaveLength(4)
    await jest.advanceTimersByTimeAsync(5000)
    await Promise.all(attempts)
  } finally {
    for (const tx of transactions) if (tx.used) tx.onabort?.()
    jest.useRealTimers()
  }
})

test('a queueing exception does not free admission before the transaction aborts', async () => {
  jest.useFakeTimers()
  const { database, transactions } = stalledDatabase('abort-settlement-admission-test', true)
  try {
    const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
    const controller = await createWebContinuationRecordingController({ database, locks: admissionLocks() })
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'b'.repeat(64) }
    const attempts = []
    for (const id of ['b', 'c', 'd']) {
      attempts.push(controller.admitClearAttemptV2({ ...input, requestId: id.repeat(64) }).catch(error => error))
      await flush()
      await jest.advanceTimersByTimeAsync(0)
      await flush()
    }
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    expect(transactions).toHaveLength(4)
    await jest.advanceTimersByTimeAsync(5000)
    await Promise.all(attempts)
  } finally {
    for (const tx of transactions) if (tx.used) tx.onabort?.()
    jest.useRealTimers()
  }
})

test('custom partitions enforce their configured limit and can change policy after settlement', async () => {
  jest.useFakeTimers()
  const { database, transactions } = stalledDatabase('configurable-admission-test')
  try {
    const { createWebRecordingPartition } = require('../../src/web/web-recording-partition')
    const locks = admissionLocks()
    const partition = limit =>
      createWebRecordingPartition({
        database,
        locks,
        clearAdmissionLimit: limit,
        crypto: { getRandomValues: bytes => bytes },
        digest: async () => 'a'.repeat(64)
      })
    const first = await partition(1).createController()
    const next = await partition(2).createController()
    const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: '1'.repeat(64) }
    const waiting = first.admitClearAttemptV2(input).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(0)
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)
    await expect(next.admitClearAttemptV2({ ...input, requestId: '2'.repeat(64) })).rejects.toMatchObject({
      code: 'capability.unavailable'
    })
    expect(transactions.filter(tx => tx.used)).toHaveLength(1)
    transactions.find(tx => tx.used).onabort()
    await waiting
    await flush()
    const retry = next.admitClearAttemptV2({ ...input, requestId: '2'.repeat(64) }).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(0)
    await flush()
    expect(transactions.filter(tx => tx.used)).toHaveLength(2)
    transactions.filter(tx => tx.used)[1].onabort()
    await retry
  } finally {
    jest.useRealTimers()
  }
})
