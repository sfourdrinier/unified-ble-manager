const { boundedWebClearAdmission } = require('../../src/web/web-clear-admission-bound')

function coordinator() {
  const held = new Map()
  return {
    async request(name, options, callback) {
      const existing = held.get(name) || []
      if (existing.length && (options.mode === 'exclusive' || existing.some(entry => entry.mode === 'exclusive')))
        return callback(null)
      const entry = { name, mode: options.mode }
      held.set(name, [...existing, entry])
      try {
        return await callback(entry)
      } finally {
        const remaining = (held.get(name) || []).filter(value => value !== entry)
        if (remaining.length) held.set(name, remaining)
        else held.delete(name)
      }
    },
    async query() {
      return { held: [...held.values()].flat(), pending: [] }
    },
    dropAll() {
      held.clear()
    }
  }
}
function deferred() {
  let resolve
  const promise = new Promise(done => {
    resolve = done
  })
  return { promise, resolve }
}
const flush = async () => {
  for (let i = 0; i < 100; i++) await Promise.resolve()
}
function database() {
  const transactions = []
  return {
    transactions,
    transaction() {
      const tx = { oncomplete: null, onabort: null, error: null, abort() {} }
      transactions.push(tx)
      return tx
    }
  }
}

test('slot remains held after caller timeout until the actual transaction settles', async () => {
  jest.useFakeTimers()
  const locks = coordinator(),
    db = database(),
    settlement = deferred()
  let starts = 0
  const operation = own => {
    starts++
    own(settlement.promise)
    return Promise.reject(new Error('queue failed'))
  }
  const first = boundedWebClearAdmission('a', locks, db, 1, operation).catch(error => error)
  await flush()
  expect(db.transactions).toHaveLength(1)
  db.transactions[0].oncomplete()
  await flush()
  expect(starts).toBe(1)
  await first
  const second = boundedWebClearAdmission('a', locks, db, 1, operation).catch(error => error)
  await flush()
  expect(starts).toBe(1)
  await jest.advanceTimersByTimeAsync(5000)
  await second
  settlement.resolve({ state: 'aborted', error: null })
  await flush()
  const third = boundedWebClearAdmission('a', locks, db, 1, operation).catch(error => error)
  await flush()
  expect(db.transactions).toHaveLength(2)
  db.transactions[1].oncomplete()
  await flush()
  expect(starts).toBe(2)
  await third
  jest.useRealTimers()
})

test('conflicting active policy is rejected and a new policy works after settlement', async () => {
  const locks = coordinator(),
    db = database(),
    settlement = deferred()
  let starts = 0
  const first = boundedWebClearAdmission('b', locks, db, 2, own => {
    own(settlement.promise)
    starts++
    return Promise.resolve('ok')
  })
  await flush()
  db.transactions[0].oncomplete()
  await first
  await flush()
  const conflict = await boundedWebClearAdmission('b', locks, db, 3, () => {
    starts++
    return Promise.resolve()
  }).catch(error => error)
  expect(conflict).toBeInstanceOf(Error)
  expect(starts).toBe(1)
  expect(db.transactions).toHaveLength(1)
  settlement.resolve({ state: 'complete' })
  await flush()
  const next = boundedWebClearAdmission('b', locks, db, 3, () => {
    starts++
    return Promise.resolve('new')
  })
  await flush()
  db.transactions[1].oncomplete()
  await expect(next).resolves.toBe('new')
  expect(starts).toBe(2)
})

test('after a realm loses its locks, a replacement waits for the overlapping IDB fence', async () => {
  const locks = coordinator(),
    db = database(),
    previous = deferred()
  let starts = 0
  const old = boundedWebClearAdmission('death', locks, db, 1, own => {
    own(previous.promise)
    starts++
    return Promise.resolve('old reply')
  })
  await flush()
  db.transactions[0].oncomplete()
  await old
  await flush()
  locks.dropAll() // model lock release when the old realm dies
  const replacement = boundedWebClearAdmission('death', locks, db, 1, () => {
    starts++
    return Promise.resolve('new reply')
  })
  await flush()
  expect(db.transactions).toHaveLength(2)
  expect(starts).toBe(1)
  // In IndexedDB the fence cannot complete until the preceding readwrite
  // transaction in readerReceipts has ended.
  previous.resolve({ state: 'aborted', error: null })
  await flush()
  db.transactions[1].oncomplete()
  await expect(replacement).resolves.toBe('new reply')
  expect(starts).toBe(2)
})

test('a caller timeout during the fence never dispatches the admission later', async () => {
  jest.useFakeTimers()
  try {
    const locks = coordinator(),
      db = database()
    let starts = 0
    const pending = boundedWebClearAdmission('timeout', locks, db, 1, () => {
      starts++
      return Promise.resolve()
    }).catch(error => error)
    await flush()
    await jest.advanceTimersByTimeAsync(5000)
    expect(await pending).toBeInstanceOf(Error)
    db.transactions[0].oncomplete()
    await flush()
    expect(starts).toBe(0)
  } finally {
    jest.useRealTimers()
  }
})

test('losing the coordination lock during policy inspection cannot dispatch work', async () => {
  const query = deferred(),
    db = database()
  let rejectGate,
    starts = 0
  const locks = {
    request(name, _options, callback) {
      if (name.endsWith(':gate')) {
        callback({})
        return new Promise((_resolve, reject) => {
          rejectGate = reject
        })
      }
      return Promise.resolve(callback({}))
    },
    query: () => query.promise
  }
  const attempt = boundedWebClearAdmission('gate-loss', locks, db, 1, () => {
    starts++
    return Promise.resolve()
  }).catch(error => error)
  await flush()
  rejectGate(new Error('gate lost'))
  await flush()
  query.resolve({ held: [] })
  await flush()
  expect(db.transactions).toHaveLength(0)
  expect(starts).toBe(0)
  expect(await attempt).toBeInstanceOf(Error)
})

test('loss of a slot invalidates late transaction callbacks', async () => {
  const db = database(),
    never = deferred()
  let rejectSlot, validate
  const locks = {
    request(name, _options, callback) {
      if (name.includes(':slot:')) {
        callback({})
        return new Promise((_resolve, reject) => {
          rejectSlot = reject
        })
      }
      return Promise.resolve(callback({}))
    },
    async query() {
      return { held: [] }
    }
  }
  const attempt = boundedWebClearAdmission('slot-loss', locks, db, 1, (own, check) => {
    validate = check
    own(never.promise)
    return never.promise
  }).catch(error => error)
  await flush()
  db.transactions[0].oncomplete()
  await flush()
  expect(typeof validate).toBe('function')
  rejectSlot(new Error('slot lost'))
  expect(await attempt).toBeInstanceOf(Error)
  expect(() => validate()).toThrow()
  never.resolve({ state: 'aborted', error: null })
})

test('admission refuses a lock manager that cannot inspect active policy', async () => {
  const db = database()
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  await expect(boundedWebClearAdmission('missing-query', locks, db, 2, () => Promise.resolve())).rejects.toMatchObject({
    normalized: { code: 'capability.unsupported' }
  })
  expect(db.transactions).toHaveLength(0)
})

test('large configured pools yield while probing busy slots so the caller deadline can run', async () => {
  jest.useFakeTimers()
  try {
    const db = database()
    const locks = {
      request(name, _options, callback) {
        return Promise.resolve(callback(name.includes(':slot:') ? null : {}))
      },
      async query() {
        return { held: [] }
      }
    }
    let finished = false
    const attempt = boundedWebClearAdmission('large-pool', locks, db, 200, () => Promise.resolve()).catch(
      error => error
    )
    attempt.then(() => {
      finished = true
    })
    for (let i = 0; i < 3000; i++) await Promise.resolve()
    expect(finished).toBe(false)
    await jest.advanceTimersByTimeAsync(5000)
    expect(await attempt).toBeInstanceOf(Error)
    expect(db.transactions).toHaveLength(0)
  } finally {
    jest.useRealTimers()
  }
})
