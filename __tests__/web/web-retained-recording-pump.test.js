const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRetainedRecordingPump } = require('../../src/core/web-retained-recording-pump')

function deferred() {
  let resolve
  let reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

function fixture(journal) {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const source = ingress.registerRetained(new Map([[characteristic, 'a']]), 4, 10)
  const pump = new WebRetainedRecordingPump(source, journal)
  return { ingress, characteristic, source, pump }
}

test('waits for transaction completion before acknowledging the exact head', async () => {
  const committed = deferred()
  const journal = { commit: jest.fn(() => committed.promise), retire: jest.fn(async () => {}) }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  f.ingress.admit(f.characteristic, new Uint8Array([2]))
  const pending = f.pump.runOnce()
  expect(f.pump.runOnce()).toBe(pending)
  expect(f.source.peek().id).toBe('1')
  expect(journal.retire).not.toHaveBeenCalled()
  committed.resolve()
  expect(await pending).toBe('committed')
  expect(f.source.peek().id).toBe('2')
  expect(journal.retire).toHaveBeenCalledWith('1')
})

test('reports delivery evidence only after commit and exact source acknowledgement', async () => {
  const committed = deferred()
  const acknowledged = jest.fn()
  const journal = { commit: jest.fn(() => committed.promise), retire: jest.fn(async () => {}), acknowledged }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  const pending = f.pump.runOnce()
  await Promise.resolve()
  expect(acknowledged).not.toHaveBeenCalled()
  committed.resolve()
  await pending
  expect(acknowledged).toHaveBeenCalledTimes(1)
  expect(acknowledged.mock.calls[0][0].id).toBe('1')
  expect(f.source.peek()).toBeNull()
})

test('retains the committed journal ordinal with the exact acknowledgement across a retry', async () => {
  const journal = {
    commit: jest.fn(async () => 42),
    acknowledged: jest.fn().mockRejectedValueOnce(new Error('ack unavailable')).mockResolvedValue(undefined),
    retire: jest.fn(async () => {})
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  await expect(f.pump.runOnce()).rejects.toThrow('ack unavailable')
  expect(journal.acknowledged.mock.calls[0][0]).toMatchObject({ id: '1', journalOrdinal: 42 })
  await f.pump.runOnce()
  expect(journal.acknowledged.mock.calls[1][0]).toEqual(journal.acknowledged.mock.calls[0][0])
  expect(journal.commit).toHaveBeenCalledTimes(1)
})

test('retries an ambiguous or aborted commit with the same identity and bytes', async () => {
  const journal = {
    commit: jest.fn().mockRejectedValueOnce(new Error('transaction aborted')).mockResolvedValue(undefined),
    retire: jest.fn(async () => {})
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([7]))
  await expect(f.pump.runOnce()).rejects.toThrow('transaction aborted')
  expect(f.source.peek().id).toBe('1')
  expect(journal.retire).not.toHaveBeenCalled()
  expect(await f.pump.runOnce()).toBe('committed')
  expect(journal.commit.mock.calls[1][0]).toEqual(journal.commit.mock.calls[0][0])
  expect(f.source.peek()).toBeNull()
})

test('retries identity retirement before processing the next delivery', async () => {
  const journal = {
    commit: jest.fn(async () => {}),
    retire: jest.fn().mockRejectedValueOnce(new Error('retire failed')).mockResolvedValue(undefined)
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  f.ingress.admit(f.characteristic, new Uint8Array([2]))
  await expect(f.pump.runOnce()).rejects.toThrow('retire failed')
  expect(f.source.peek().id).toBe('2')
  expect(await f.pump.runOnce()).toBe('retired')
  expect(journal.commit).toHaveBeenCalledTimes(1)
  expect(await f.pump.runOnce()).toBe('committed')
  expect(journal.commit.mock.calls[1][0].id).toBe('2')
})

test('a reentrant wake cannot commit the same head twice concurrently', async () => {
  let f
  let nested
  const journal = {
    commit: jest.fn(async () => {
      nested = f.pump.runOnce()
    }),
    retire: jest.fn(async () => {})
  }
  f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  const run = f.pump.runOnce()
  expect(await run).toBe('committed')
  expect(nested).toBe(run)
  expect(journal.commit).toHaveBeenCalledTimes(1)
})

test('authority revoked after commit prevents source acknowledgement and retirement', async () => {
  let current = true
  const journal = {
    commit: jest.fn(async () => {
      current = false
    }),
    retire: jest.fn(async () => {}),
    acknowledged: jest.fn(),
    canAcknowledge: () => current
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([8]))
  await expect(f.pump.runOnce()).rejects.toThrow(/authority/i)
  expect(f.source.peek().id).toBe('1')
  expect(journal.retire).not.toHaveBeenCalled()
  expect(journal.acknowledged).not.toHaveBeenCalled()
})

test('retains exact acknowledged delivery until asynchronous evidence is durable', async () => {
  const evidence = deferred()
  const journal = {
    commit: jest.fn(async () => {}),
    acknowledged: jest.fn(() => evidence.promise),
    retire: jest.fn(async () => {})
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  const pending = f.pump.runOnce()
  for (let i = 0; i < 5; i++) await Promise.resolve()
  expect(f.source.peek()).toBeNull()
  expect(journal.acknowledged).toHaveBeenCalledTimes(1)
  expect(journal.retire).not.toHaveBeenCalled()
  evidence.resolve()
  expect(await pending).toBe('committed')
  expect(journal.retire).toHaveBeenCalledWith('1')
})

test('retries failed acknowledgement evidence for the same delivery before retirement or next commit', async () => {
  const journal = {
    commit: jest.fn(async () => {}),
    acknowledged: jest.fn().mockRejectedValueOnce(new Error('ack evidence unavailable')).mockResolvedValue(undefined),
    retire: jest.fn(async () => {})
  }
  const f = fixture(journal)
  f.ingress.admit(f.characteristic, new Uint8Array([1]))
  f.ingress.admit(f.characteristic, new Uint8Array([2]))
  await expect(f.pump.runOnce()).rejects.toThrow('ack evidence unavailable')
  expect(f.source.peek().id).toBe('2')
  expect(journal.retire).not.toHaveBeenCalled()
  expect(await f.pump.runOnce()).toBe('retired')
  expect(journal.acknowledged.mock.calls[1][0]).toEqual(journal.acknowledged.mock.calls[0][0])
  expect(journal.commit).toHaveBeenCalledTimes(1)
  expect(journal.retire).toHaveBeenCalledWith('1')
})
