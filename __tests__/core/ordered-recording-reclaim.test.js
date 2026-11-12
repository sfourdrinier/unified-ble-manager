// __tests__/core/ordered-recording-reclaim.test.js
const { createOrderedRecordingReclaimReservations } = require('../../src/core/ordered-recording-reclaim')

test('status and cancellation reach the exact reservation while commit is hung, retaining its late receipt', async () => {
  let finishCommit
  const calls = []
  const transport = {
    next: async () => '42', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async (id, key) => { calls.push(['status', id, key]); return { state: 'pending' } },
    commit: async () => new Promise(resolve => { finishCommit = resolve }),
    cancel: async (id, key) => { calls.push(['cancel', id, key]); return { state: 'committed', mappingCommitted: true } },
    release: async () => ({ state: 'released' }), retire: async () => {}, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'exact-key')()
  await handle.start()
  const commit = handle.commit()
  while (!finishCommit) await Promise.resolve()
  const status = handle.status()
  const fence = handle.cancel()
  const outcomes = await Promise.all([status, fence])
  expect(outcomes).toEqual([{ state: 'pending' }, { state: 'committed', mappingCommitted: true }])
  expect(calls).toEqual([['status', '42', 'exact-key'], ['cancel', '42', 'exact-key']])
  finishCommit({ state: 'committed', mappingCommitted: true })
  expect(await commit).toEqual({ state: 'committed', mappingCommitted: true })
  await handle.retire()
})

test('a hung status does not block the fence or release', async () => {
  let statusStarted
  const entered = new Promise(resolve => { statusStarted = resolve })
  const transport = {
    next: async () => '7', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => { statusStarted(); return new Promise(() => {}) },
    commit: async () => ({ state: 'committed' }), cancel: async () => ({ state: 'failed', mappingCommitted: false }),
    release: async () => ({ state: 'released', mappingCommitted: false }), retire: async () => {},
    acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  await handle.start()
  void handle.status()
  await entered
  const outcomes = await Promise.all([handle.cancel(), handle.release()])
  expect(outcomes).toEqual([{ state: 'failed', mappingCommitted: false }, { state: 'released', mappingCommitted: false }])
})

test('a hung start does not prevent status and cancellation of its reserved identity', async () => {
  let startEntered
  const entered = new Promise(resolve => { startEntered = resolve })
  const transport = {
    next: async () => '9', reserve: async () => {}, start: async () => { startEntered(); return new Promise(() => {}) },
    status: async () => ({ state: 'pending' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'failed', mappingCommitted: false }), release: async () => ({ state: 'released' }),
    retire: async () => {}, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  void handle.start()
  await entered
  const outcomes = await Promise.all([handle.status(), handle.cancel()])
  expect(outcomes).toEqual([{ state: 'pending' }, { state: 'failed', mappingCommitted: false }])
})

test('a hung retirement does not block status or cancellation', async () => {
  let retireEntered
  const entered = new Promise(resolve => { retireEntered = resolve })
  const transport = {
    next: async () => '9', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'ready' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'failed', mappingCommitted: false }), release: async () => ({ state: 'released' }),
    retire: async () => { retireEntered(); return new Promise(() => {}) }, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  await handle.start()
  void handle.retire()
  await entered
  const outcomes = await Promise.all([handle.status(), handle.cancel()])
  expect(outcomes).toEqual([{ state: 'ready' }, { state: 'failed', mappingCommitted: false }])
})

test('one recorder with a hung commit does not block another recorder', async () => {
  const makeTransport = hung => ({
    next: async key => key, reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'unknown' }),
    commit: async () => hung ? new Promise(() => {}) : { state: 'committed', mappingCommitted: true },
    cancel: async () => ({ state: 'no-effect' }), release: async () => ({ state: 'released' }),
    retire: async () => {}, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  })
  const first = createOrderedRecordingReclaimReservations(makeTransport(true), () => 'first-peer')()
  const second = createOrderedRecordingReclaimReservations(makeTransport(false), () => 'second-peer')()
  await Promise.all([first.start(), second.start()])
  void first.commit()
  expect(await first.status()).toEqual({ state: 'unknown' })
  expect(await first.cancel()).toEqual({ state: 'no-effect' })
  expect(await second.commit()).toEqual({ state: 'committed', mappingCommitted: true })
  await second.retire()
})

test('replays the exact reserved identity after a lost response and serializes commit and release', async () => {
  const calls = []
  let failReserve = true
  const transport = {
    next: async key => { calls.push(['next', key]); return '12' },
    reserve: async (id, key) => { calls.push(['reserve', id, key]); if (failReserve) { failReserve = false; throw new Error('lost response') } },
    start: async (id, key) => { calls.push(['start', id, key]); return { state: 'ready' } },
    status: async (id, key) => { calls.push(['status', id, key]); return { state: 'ready' } },
    commit: async (id, key) => { calls.push(['commit', id, key]); return { state: 'committed' } },
    cancel: async () => ({ state: 'released' }),
    release: async (id, key) => { calls.push(['release', id, key]); return { state: 'released' } },
    retire: async (id, key) => { calls.push(['retire', id, key]) },
    acknowledgeRetirement: async (id, key) => { calls.push(['ack', id, key]) },
    forgetRetirement: async (id, key) => { calls.push(['forget', id, key]) }
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'unique-key')
  const handle = reserve()
  await expect(handle.start()).rejects.toThrow('lost response')
  expect(() => reserve()).toThrow('lifecycle.invalid-state')
  await expect(handle.start()).resolves.toEqual({ state: 'ready' })
  const commit = handle.commit()
  const release = handle.release()
  await commit
  await release
  await handle.retire()
  await handle.retire()
  expect(calls).toEqual([
    ['next', 'unique-key'], ['reserve', '12', 'unique-key'], ['reserve', '12', 'unique-key'],
    ['start', '12', 'unique-key'], ['commit', '12', 'unique-key'], ['release', '12', 'unique-key'], ['retire', '12', 'unique-key'], ['ack', '12', 'unique-key'], ['forget', '12', 'unique-key']
  ])
})

test('cancel before start and retirement perform no host effects', async () => {
  const call = jest.fn()
  const transport = { next: call, reserve: call, start: call, status: call, commit: call, cancel: call, release: call, retire: call, acknowledgeRetirement: call, forgetRetirement: call }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  await expect(handle.cancel()).resolves.toEqual({ state: 'cancelled' })
  await expect(handle.start()).resolves.toEqual({ state: 'cancelled' })
  await expect(handle.commit()).rejects.toThrow('lifecycle.invalid-state')
  await expect(handle.release()).rejects.toThrow('lifecycle.invalid-state')
  await handle.retire()
  expect(call).not.toHaveBeenCalled()
})

test('lost retire response blocks a new reclaim until the exact retirement is retried', async () => {
  let lost = true
  const retire = jest.fn(async () => { if (lost) { lost = false; throw new Error('lost retire response') } })
  const transport = {
    next: async () => '1', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'ready' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'released' }), release: async () => ({ state: 'released' }), retire, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'exact-key')
  const first = reserve()
  await first.start()
  await first.cancel()
  await expect(first.retire()).rejects.toThrow('lost retire response')
  expect(() => reserve()).toThrow('lifecycle.invalid-state')
  await first.retire()
  expect(retire).toHaveBeenCalledTimes(2)
  expect(retire.mock.calls).toEqual([['1', 'exact-key'], ['1', 'exact-key']])
  expect(await first.status()).toEqual({ state: 'retired' })
  expect(reserve()).toBeDefined()
})

test('lost retirement acknowledgement is retried before another handle is admitted', async () => {
  const retire = jest.fn(async () => {})
  let lost = true
  const acknowledgeRetirement = jest.fn(async () => { if (lost) { lost = false; throw new Error('lost ack') } })
  const transport = {
    next: async () => '9', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'ready' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'released' }), release: async () => ({ state: 'released' }), retire, acknowledgeRetirement, forgetRetirement: async () => {}
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'key')
  const handle = reserve()
  await handle.start()
  await handle.cancel()
  await expect(handle.retire()).rejects.toThrow('lost ack')
  expect(await handle.status()).toEqual({ state: 'ready' })
  expect(() => reserve()).toThrow('lifecycle.invalid-state')
  await handle.retire()
  expect(retire).toHaveBeenCalledTimes(1)
  expect(acknowledgeRetirement.mock.calls).toEqual([['9', 'key'], ['9', 'key']])
  expect(reserve()).toBeDefined()
})

test('lost final forget response retains terminal handle and blocks another reclaim', async () => {
  const retire = jest.fn(async () => {})
  const acknowledgeRetirement = jest.fn(async () => {})
  let lost = true
  const forgetRetirement = jest.fn(async () => { if (lost) { lost = false; throw new Error('lost forget') } })
  const transport = {
    next: async () => '8', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'ready' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'released' }), release: async () => ({ state: 'released' }), retire, acknowledgeRetirement, forgetRetirement
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'key')
  const handle = reserve()
  await handle.start()
  await handle.cancel()
  await expect(handle.retire()).rejects.toThrow('lost forget')
  expect(await handle.status()).toEqual({ state: 'ready' })
  expect(() => reserve()).toThrow('lifecycle.invalid-state')
  await handle.retire()
  expect(retire).toHaveBeenCalledTimes(1)
  expect(acknowledgeRetirement).toHaveBeenCalledTimes(1)
  expect(forgetRetirement.mock.calls).toEqual([['8', 'key'], ['8', 'key']])
  expect(reserve()).toBeDefined()
})

test('lost forget acknowledgement retries the exact acknowledgement without repeating forget', async () => {
  const forgetRetirement = jest.fn(async () => {})
  let lost = true
  const acknowledgeForget = jest.fn(async () => { if (lost) { lost = false; throw new Error('lost forget acknowledgement') } })
  const transport = {
    next: async () => '9', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'ready' }), commit: async () => ({}),
    cancel: async () => ({ state: 'released' }), release: async () => ({ state: 'released' }),
    retire: async () => {}, acknowledgeRetirement: async () => {}, forgetRetirement, acknowledgeForget
  }
  const make = createOrderedRecordingReclaimReservations(transport, () => 'exact-key')
  const handle = make()
  await handle.start()
  await expect(handle.retire()).rejects.toThrow('lost forget acknowledgement')
  expect(() => make()).toThrow()
  await handle.retire()
  expect(forgetRetirement).toHaveBeenCalledTimes(1)
  expect(acknowledgeForget.mock.calls).toEqual([['9', 'exact-key'], ['9', 'exact-key']])
  expect(() => make()).not.toThrow()
})

test('a premature retirement refusal still permits the required native cancellation', async () => {
  let released = false
  const transport = {
    next: async () => '2', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: released ? 'released' : 'ready' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => { released = true; return { state: 'released' } }, release: async () => ({ state: 'released' }),
    retire: async () => { if (!released) throw new Error('cleanup pending') }, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  await handle.start()
  await expect(handle.retire()).rejects.toThrow('cleanup pending')
  await expect(handle.start()).rejects.toThrow('lifecycle.invalid-state')
  await expect(handle.cancel()).resolves.toEqual({ state: 'released' })
  await handle.retire()
})

test('lost allocation response is recovered before cancellation and retirement', async () => {
  let lost = true
  const calls = []
  const transport = {
    next: async key => { calls.push(['next', key]); if (lost) { lost = false; throw new Error('lost allocation') } return '5' },
    reserve: async (id, key) => { calls.push(['reserve', id, key]) },
    start: async () => ({ state: 'pending' }), status: async () => ({ state: 'reserved' }),
    commit: async () => ({ state: 'committed' }), cancel: async (id, key) => { calls.push(['cancel', id, key]); return { state: 'failed' } },
    release: async () => ({ state: 'released' }), retire: async (id, key) => { calls.push(['retire', id, key]) },
    acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'original-key')()
  await expect(handle.start()).rejects.toThrow('lost allocation')
  await expect(handle.cancel()).resolves.toEqual({ state: 'failed' })
  await handle.retire()
  expect(calls).toEqual([['next', 'original-key'], ['next', 'original-key'], ['reserve', '5', 'original-key'], ['cancel', '5', 'original-key'], ['retire', '5', 'original-key']])
})

test('authoritative no-reservation refusal remains visible and retires without native cleanup', async () => {
  const { contractError } = require('../../src/backend-contract/errors')
  const refusal = { state: 'not-reserved', error: contractError('gatt.stale-handle', 'gatt', 'reclaim').normalized }
  const effect = jest.fn()
  const transport = { next: async () => refusal, reserve: effect, start: effect, status: effect, commit: effect, cancel: effect, release: effect, retire: effect, acknowledgeRetirement: effect, forgetRetirement: effect }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'key')
  const handle = reserve()
  expect(await handle.start()).toEqual(refusal)
  expect(await handle.status()).toEqual(refusal)
  expect(await handle.cancel()).toEqual(refusal)
  await expect(handle.commit()).rejects.toThrow('lifecycle.invalid-state')
  await expect(handle.release()).rejects.toThrow('lifecycle.invalid-state')
  await handle.retire()
  expect(effect).not.toHaveBeenCalled()
  expect(reserve()).toBeDefined()
})

test('a competing reservation refusal retires without touching the winning owner', async () => {
  const { contractError } = require('../../src/backend-contract/errors')
  const refusal = { state: 'not-reserved', error: contractError('lifecycle.invalid-state', 'cleanup', 'reclaim').normalized }
  const cleanup = jest.fn()
  const transport = {
    next: async () => '7', reserve: async () => refusal, start: cleanup, status: cleanup,
    commit: cleanup, cancel: cleanup, release: cleanup, retire: cleanup,
    acknowledgeRetirement: cleanup, forgetRetirement: cleanup
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'loser')
  const handle = reserve()
  expect(await handle.start()).toEqual(refusal)
  expect(await handle.status()).toEqual(refusal)
  await handle.retire()
  expect(cleanup).not.toHaveBeenCalled()
  expect(reserve()).toBeDefined()
})

test('host-level refusal preserves the outer reservation until explicit retirement', async () => {
  const { contractError } = require('../../src/backend-contract/errors')
  const refusal = { state: 'not-reserved', error: contractError('gatt.stale-handle', 'gatt', 'reclaim').normalized }
  const retire = jest.fn(async () => {})
  const transport = {
    next: async () => '3', reserve: async () => {}, start: async () => refusal, status: async () => refusal,
    commit: async () => ({ state: 'committed' }), cancel: async () => ({ state: 'failed' }), release: async () => ({ state: 'released' }),
    retire, acknowledgeRetirement: async () => {}, forgetRetirement: async () => {}
  }
  const handle = createOrderedRecordingReclaimReservations(transport, () => 'key')()
  expect(await handle.start()).toEqual(refusal)
  expect(await handle.status()).toEqual(refusal)
  await handle.retire()
  expect(retire).toHaveBeenCalledWith('3', 'key')
})

test('native retirement is not reported as complete until the forget receipt is recovered', async () => {
  let lost = true
  const transport = {
    next: async () => '1', reserve: async () => {}, start: async () => ({ state: 'ready' }),
    status: async () => ({ state: 'retired' }), commit: async () => ({ state: 'committed' }),
    cancel: async () => ({ state: 'released' }), release: async () => ({ state: 'released' }),
    retire: async () => {}, acknowledgeRetirement: async () => {},
    forgetRetirement: async () => { if (lost) { lost = false; throw new Error('lost forget') } }
  }
  const reserve = createOrderedRecordingReclaimReservations(transport, () => 'key')
  const handle = reserve()
  await handle.start()
  await expect(handle.retire()).rejects.toThrow('lost forget')
  expect(await handle.status()).toEqual({ state: 'retiring' })
  expect(() => reserve()).toThrow('lifecycle.invalid-state')
  await handle.retire()
  expect(await handle.status()).toEqual({ state: 'retired' })
})
