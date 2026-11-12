const { runWebIndexedDbTransaction, ownWebIndexedDbTransaction } = require('../../src/web/web-indexeddb-transaction')

function transaction() {
  return { error: null, objectStore: jest.fn(() => ({})), abort: jest.fn(), oncomplete: null, onabort: null }
}

test('request success does not resolve until the transaction completes', async () => {
  const tx = transaction()
  const db = { transaction: jest.fn(() => tx) }
  let settled = false
  const result = runWebIndexedDbTransaction(db, ['records'], 'readwrite', () => 'request-success')
  result.then(() => {
    settled = true
  })
  await Promise.resolve()
  expect(settled).toBe(false)
  tx.oncomplete()
  expect(await result).toEqual({ value: 'request-success', durability: 'strict' })
  expect(db.transaction).toHaveBeenCalledWith(['records'], 'readwrite', { durability: 'strict' })
})

test('a transaction abort after request success rejects and never reports a commit', async () => {
  const tx = transaction()
  const result = runWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => 1)
  tx.error = new Error('quota')
  tx.onabort()
  await expect(result).rejects.toThrow('quota')
})

test('falls back only when strict transaction options are unsupported', async () => {
  const tx = transaction()
  const db = {
    transaction: jest
      .fn()
      .mockImplementationOnce(() => {
        throw new TypeError('unsupported option')
      })
      .mockReturnValueOnce(tx)
  }
  const result = runWebIndexedDbTransaction(db, ['records'], 'readwrite', () => 1)
  tx.oncomplete()
  expect(await result).toEqual({ value: 1, durability: 'default' })
  expect(db.transaction).toHaveBeenCalledTimes(2)
  expect(db.transaction).toHaveBeenLastCalledWith(['records'], 'readwrite')
  const denied = new Error('denied')
  expect(() =>
    runWebIndexedDbTransaction(
      {
        transaction: () => {
          throw denied
        }
      },
      ['records'],
      'readwrite',
      () => 1
    )
  ).toThrow(denied)
})

test('aborts when synchronously queuing the transaction requests fails', async () => {
  const tx = transaction()
  const result = runWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => {
    throw new Error('bad request')
  })
  expect(tx.abort).toHaveBeenCalledTimes(1)
  await expect(result).rejects.toThrow('bad request')
})

test('queue failure is visible before abort settlement, and request errors do not settle ownership', async () => {
  const tx = transaction()
  const owner = ownWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => {
    throw new Error('queue failed')
  })
  let terminal = false
  owner.settled.then(() => {
    terminal = true
  })
  await expect(owner.result).rejects.toThrow('queue failed')
  tx.onerror?.({ target: { error: new Error('request failed') } })
  await Promise.resolve()
  expect(terminal).toBe(false)
  tx.onabort()
  await expect(owner.settled).resolves.toEqual({ state: 'aborted', error: null })
})

test('ownership reports completion or failure to dispatch separately', async () => {
  const tx = transaction()
  const owner = ownWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => 7)
  tx.oncomplete()
  await expect(owner.result).resolves.toEqual({ value: 7, durability: 'strict' })
  await expect(owner.settled).resolves.toEqual({ state: 'complete' })
  const failure = new Error('dispatch denied')
  const missing = ownWebIndexedDbTransaction(
    {
      transaction: () => {
        throw failure
      }
    },
    ['records'],
    'readwrite',
    () => 1
  )
  await expect(missing.result).rejects.toBe(failure)
  await expect(missing.settled).resolves.toEqual({ state: 'not-started' })
})

test('a failed abort request retains ownership until the actual completion event', async () => {
  const tx = transaction()
  tx.abort.mockImplementation(() => {
    throw new Error('abort failed')
  })
  const owner = ownWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => {
    throw new Error('queue failed')
  })
  let terminal = false
  owner.settled.then(() => {
    terminal = true
  })
  await expect(owner.result).rejects.toThrow('Failed to queue and abort')
  await Promise.resolve()
  expect(terminal).toBe(false)
  tx.oncomplete()
  await expect(owner.settled).resolves.toEqual({ state: 'complete' })
})

test('abort settlement retains the browser transaction error', async () => {
  const tx = transaction()
  const error = new Error('quota source')
  tx.error = error
  const owner = ownWebIndexedDbTransaction({ transaction: () => tx }, ['records'], 'readwrite', () => 1)
  tx.onabort()
  await expect(owner.result).rejects.toBe(error)
  await expect(owner.settled).resolves.toEqual({ state: 'aborted', error })
})

test('readiness rejection releases only the undispatched ownership', async () => {
  const failure = new Error('not ready')
  const db = { ready: () => Promise.reject(failure), transaction: jest.fn() }
  const owner = ownWebIndexedDbTransaction(db, ['records'], 'readwrite', () => 1)
  await expect(owner.result).rejects.toBe(failure)
  await expect(owner.settled).resolves.toEqual({ state: 'not-started' })
  expect(db.transaction).not.toHaveBeenCalled()
})
