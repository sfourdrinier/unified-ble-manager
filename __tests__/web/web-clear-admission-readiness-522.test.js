const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')

const flush = async () => {
  for (let i = 0; i < 150; i++) await Promise.resolve()
}
function deferred() {
  let resolve
  const promise = new Promise(done => {
    resolve = done
  })
  return { promise, resolve }
}
function locksWithSlotLoss() {
  const held = new Map()
  let loseSlot
  return {
    request(name, options, callback) {
      const existing = held.get(name) || []
      if (existing.length && (options.mode === 'exclusive' || existing.some(entry => entry.mode === 'exclusive')))
        return Promise.resolve(callback(null))
      const entry = { name, mode: options.mode }
      held.set(name, [...existing, entry])
      let rejectLoss
      const loss = new Promise((_resolve, reject) => {
        rejectLoss = reject
      })
      if (name.includes(':slot:')) loseSlot = () => rejectLoss(new Error('slot lost'))
      return Promise.race([Promise.resolve().then(() => callback(entry)), loss]).finally(() => {
        const remaining = (held.get(name) || []).filter(value => value !== entry)
        if (remaining.length) held.set(name, remaining)
        else held.delete(name)
      })
    },
    async query() {
      return { held: [...held.values()].flat() }
    },
    loseSlot() {
      loseSlot()
    }
  }
}
function fixture() {
  const delayed = deferred(),
    transactions = []
  let readiness = 0
  const database = {
    name: 'admission-readiness-522',
    ready() {
      readiness++
      return readiness === 2 ? delayed.promise : Promise.resolve()
    },
    transaction() {
      let used = false
      const tx = {
        oncomplete: null,
        onabort: null,
        error: null,
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
      Promise.resolve().then(() => {
        if (!used) tx.oncomplete?.()
      })
      return tx
    }
  }
  return {
    database,
    delayed,
    transactions,
    get readiness() {
      return readiness
    }
  }
}
const input = { recordingId: 'sample', producerIncarnation: 'producer', requestId: 'a'.repeat(64) }

test('lost slot and replacement fence forbid a delayed admission transaction', async () => {
  const f = fixture(),
    locks = locksWithSlotLoss()
  const controller = await createWebContinuationRecordingController({
    database: f.database,
    locks,
    clearAdmissionLimit: 1
  })
  const old = controller.admitClearAttemptV2(input).catch(error => error)
  await flush()
  expect(f.readiness).toBe(2)
  expect(f.transactions).toHaveLength(1)
  locks.loseSlot()
  expect((await old).code).toBe('capability.unavailable')
  await flush()
  const replacement = controller.admitClearAttemptV2(input).catch(error => error)
  await flush()
  expect(f.transactions).toHaveLength(3)
  expect(f.transactions[2].used).toBe(true)
  f.delayed.resolve()
  await flush()
  expect(f.transactions).toHaveLength(3)
  f.transactions[2].onabort()
  await replacement
})

test('timed-out readiness cannot dispatch later and retains its slot until readiness settles', async () => {
  jest.useFakeTimers()
  try {
    const f = fixture(),
      locks = locksWithSlotLoss()
    const controller = await createWebContinuationRecordingController({
      database: f.database,
      locks,
      clearAdmissionLimit: 1
    })
    const old = controller.admitClearAttemptV2(input).catch(error => error)
    await flush()
    expect(f.readiness).toBe(2)
    await jest.advanceTimersByTimeAsync(5000)
    expect((await old).code).toBe('capability.unavailable')
    for (let i = 0; i < 3; i++) {
      expect((await controller.admitClearAttemptV2(input).catch(error => error)).code).toBe('capability.unavailable')
    }
    expect(f.transactions).toHaveLength(1)
    f.delayed.resolve()
    await flush()
    expect(f.transactions).toHaveLength(1)
    const retry = controller.admitClearAttemptV2(input).catch(error => error)
    await flush()
    expect(f.transactions).toHaveLength(3)
    expect(f.transactions[2].used).toBe(true)
    f.transactions[2].onabort()
    await retry
  } finally {
    jest.useRealTimers()
  }
})
