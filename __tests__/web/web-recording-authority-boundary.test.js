const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')

function locks() {
  const held = new Set()
  return {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
}
const digest = async () => 'ab'.repeat(32)
const withIncarnation = authority => ({
  ...declaration,
  producerIncarnation: authority.createProducerIncarnation(require('crypto').webcrypto)
})
const declaration = {
  id: 'id',
  maxBytes: 1048576,
  maxRecords: 2,
  context: {
    kind: 'ordered-recording',
    version: 1,
    peerId: 'p',
    connectionGeneration: 'c',
    databaseGeneration: 'd',
    maxItems: 8,
    maxBytes: 1024,
    maxControls: 4,
    members: [
      {
        memberId: '0',
        publicMemberId: 'x',
        selector: {
          serviceUuid: '180d',
          serviceOccurrence: '0',
          characteristicUuid: '2a37',
          characteristicOccurrence: '0'
        },
        delivery: 'notification',
        preference: null
      }
    ]
  },
  producerIncarnation: 'producer',
  collectionId: '1',
  sourceGeneration: 'source',
  memberEpochs: [{ member: '0', epoch: '1' }]
}
const value = {
  kind: 'value',
  ordinal: '1',
  member: '0',
  membership: { member: '0', epoch: '1' },
  bytes: new Uint8Array([1]),
  delivery: 'notification',
  provenance: 'read-or-notification'
}

async function releasedInterrupted(db, authority, sourceDeclaration, append = false) {
  await authority.declare(sourceDeclaration)
  const journal = createWebOrderedRecordingJournal(db, { ...sourceDeclaration, ownerEpoch: '1' }, authority.reservationOwner())
  await journal.open('c'.repeat(64))
  authority.noteOpenCommitted()
  if (append) await journal.commit({ id: '1', record: value })
  await authority.beginCleanup()
  // Model an interruption recorded after the former source has been cleaned up.
  db.mutate('recordings', row => { row.ownerState = 'interrupted'; row.uncertainTail = true })
  await authority.release()
  return journal
}

test('exclusive owner declares atomically and released interruption fences the old journal', async () => {
  const db = transactionalDatabase(),
    manager = locks()
  const opts = { database: db, locks: manager, digest }
  const [first, competing] = await Promise.all([
    createWebRecordingAuthority(opts, 'id'),
    createWebRecordingAuthority(opts, 'id')
  ])
  expect(first.state).toBe('acquired')
  expect(competing).toMatchObject({ state: 'busy', lastCommitted: { state: 'missing' } })
  const sourceDeclaration = withIncarnation(first.authority)
  const initial = await first.authority.declare(sourceDeclaration)
  expect(initial).toMatchObject({ state: 'prepared', ownerEpoch: '1' })
  expect(db.rows('recordings')[0].readerIntegrity).toEqual({
    version: 3,
    receipt: null,
    accountedBytes: 0,
    pendingCount: 0,
    pendingBytes: 0,
    deliveryId: null,
    sourceOrdinal: null
  })
  const journal = createWebOrderedRecordingJournal(db, { ...sourceDeclaration, ownerEpoch: initial.ownerEpoch }, first.authority.reservationOwner())
  await journal.declare()
  await journal.open('c'.repeat(64))
  first.authority.noteOpenCommitted()
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({
    state: 'busy',
    lastCommitted: { state: 'live', ownerEpoch: '1' }
  })
  await first.authority.beginCleanup()
  db.mutate('recordings', row => { row.ownerState = 'interrupted'; row.uncertainTail = true })
  await first.authority.release()
  const next = await createWebRecordingAuthority(opts, 'id')
  const recovered = await next.authority.recover()
  expect(recovered).toMatchObject({ state: 'interrupted', ownerEpoch: '1', uncertainTail: true })
  expect(next.authority.reservationOwner()).toMatchObject({ generation: '2', ownerEpoch: '1' })
  await expect(journal.commit({ id: '1', record: value })).rejects.toThrow()
  expect(db.rows('records')).toEqual([])
  await next.authority.release()
})

test('failed declaration is not reported as durable and retains authority for cleanup', async () => {
  const db = transactionalDatabase(),
    manager = locks()
  const owner = (await createWebRecordingAuthority({ database: db, locks: manager, digest }, 'id')).authority
  db.abortNext(new Error('storage failure'))
  await expect(owner.declare(withIncarnation(owner))).rejects.toThrow('storage failure')
  expect(db.rows('recordings')).toEqual([])
  expect(owner.current()).toBe(true)
  await owner.release()
})

test('a released prepared journal cannot be cold recovered but may be rebound before source open', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  await first.declare(withIncarnation(first))
  await first.release()
  const second = (await createWebRecordingAuthority(opts, 'id')).authority
  await expect(second.recover()).rejects.toThrow('reservation-recovery-state')
  expect(await second.declare(withIncarnation(second))).toMatchObject({ state: 'prepared', ownerEpoch: '2' })
  await second.release()
  expect(db.rows('recordings')[0]).toMatchObject({ ownerState: 'prepared', ownerEpoch: '2' })
})

test('cleanup failure retains exclusive ownership and permits a retry', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  await expect(first.close(() => Promise.reject(new Error('cleanup failed')))).rejects.toThrow('cleanup failed')
  expect(first.current()).toBe(true)
  expect(await createWebRecordingAuthority(opts, 'id')).toMatchObject({ state: 'busy' })
  await first.close(() => Promise.resolve())
  const second = (await createWebRecordingAuthority(opts, 'id')).authority
  await second.release()
})

test('release fences new work and retains the lock until recovery commits', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  await releasedInterrupted(db, first, withIncarnation(first))
  const owner = (await createWebRecordingAuthority(opts, 'id')).authority
  db.pauseNextCompletion()
  const recovery = owner.recover()
  await db.requestsProcessed()
  const closing = owner.release()
  expect(owner.current()).toBe(false)
  expect(await require('../../src/web/web-recording-lock').acquireWebRecordingLock(manager, digest, 'id')).toEqual({
    state: 'busy'
  })
  db.complete()
  expect(await recovery).toMatchObject({ state: 'interrupted', ownerEpoch: '1' })
  await closing
  const next = (await createWebRecordingAuthority(opts, 'id')).authority
  expect(await next.recover()).toMatchObject({ state: 'interrupted', ownerEpoch: '1' })
  expect(next.reservationOwner()).toMatchObject({ generation: '3', ownerEpoch: '1' })
  await next.release()
})

test('abort settles while coordinator selection is still in flight', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    controller = new AbortController()
  db.pauseNextCompletion()
  const pending = createWebRecordingAuthority({ database: db, locks: manager, digest, signal: controller.signal }, 'id')
  await db.requestsProcessed()
  controller.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  db.complete()
  const next = (await createWebRecordingAuthority({ database: db, locks: manager, digest }, 'id')).authority
  await next.release()
})

test('release waits for in-flight restoration validation', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const owner = (await createWebRecordingAuthority(opts, 'id')).authority
  await owner.declare(withIncarnation(owner))
  db.pauseNextCompletion()
  const validation = owner.revalidate(() => true)
  await db.requestsProcessed()
  const closing = owner.release()
  expect(await require('../../src/web/web-recording-lock').acquireWebRecordingLock(manager, digest, 'id')).toEqual({
    state: 'busy'
  })
  db.complete()
  await validation
  await closing
})

test('restoration validates the durable epoch and actual graph before admission', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    onLoss = jest.fn()
  const authority = (await createWebRecordingAuthority({ database: db, locks: manager, digest, onLoss }, 'id'))
    .authority
  await authority.declare(withIncarnation(authority))
  db.pauseNextCompletion()
  const check = authority.revalidate(() => true)
  expect(authority.current()).toBe(false)
  await db.requestsProcessed()
  db.complete()
  await check
  expect(authority.current()).toBe(true)
  db.mutate('recordings', row => {
    row.ownerEpoch = '2'
  })
  await expect(authority.revalidate(() => true)).rejects.toThrow()
  expect(authority.current()).toBe(false)
  expect(onLoss).toHaveBeenCalledTimes(1)
  await expect(authority.release()).rejects.toThrow()
  expect(db.rows('authorityReservations')[0]).toMatchObject({ phase: 'provisional', ownerEpoch: '1' })
  db.mutate('recordings', row => { row.ownerEpoch = '1' })
  await authority.release()
})

test('recovery abort does not advance an epoch or claim interruption', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  const sourceDeclaration = withIncarnation(first)
  await releasedInterrupted(db, first, sourceDeclaration, true)
  const second = (await createWebRecordingAuthority(opts, 'id')).authority
  const before = ['recordings', 'records', 'pendingIdentities', 'readerReceipts'].map(name => db.rows(name))
  db.abortNext(new Error('abort recovery'))
  await expect(second.recover()).rejects.toThrow('abort recovery')
  expect(['recordings', 'records', 'pendingIdentities', 'readerReceipts'].map(name => db.rows(name))).toEqual(before)
  expect(await second.recover()).toMatchObject({ state: 'interrupted', ownerEpoch: '1', uncertainTail: true })
  expect(second.reservationOwner()).toMatchObject({ generation: '2', ownerEpoch: '1' })
  await second.release()
})

test.each([
  [
    'unsupported version',
    row => {
      row.readerIntegrity.version = 99
    }
  ],
  [
    'missing integrity',
    row => {
      delete row.readerIntegrity
    }
  ],
  [
    'malformed integrity counter',
    row => {
      row.readerIntegrity.pendingCount = -1
    }
  ],
  [
    'mismatched pending count',
    row => {
      row.pendingCount = 0
    }
  ],
  [
    'mismatched pending bytes',
    row => {
      row.pendingBytes = 1
    }
  ],
  [
    'malformed identity evidence',
    row => {
      row.readerIntegrity.deliveryId = 'not-an-ordinal'
    }
  ],
  [
    'malformed source ordinal evidence',
    row => {
      row.readerIntegrity.sourceOrdinal = '01'
    }
  ]
])('authority selection rejects %s without mutating stored evidence', async (_name, corrupt) => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  const sourceDeclaration = withIncarnation(first)
  await releasedInterrupted(db, first, sourceDeclaration, true)
  db.mutate('recordings', corrupt)
  const stores = ['recordings', 'records', 'pendingIdentities', 'readerReceipts', 'authorityReservations']
  const before = stores.map(name => db.rows(name))
  const code = _name === 'mismatched pending count' ? 'lifecycle.invariant-violation' : 'protocol.malformed'
  await expect(createWebRecordingAuthority(opts, 'id')).rejects.toMatchObject({ normalized: { code } })
  expect(stores.map(name => db.rows(name))).toEqual(before)
})

test('missing rows stay missing and mismatched epochs fail without mutation', async () => {
  const db = transactionalDatabase(),
    manager = locks(),
    opts = { database: db, locks: manager, digest }
  const first = (await createWebRecordingAuthority(opts, 'id')).authority
  expect(await first.recover()).toMatchObject({ state: 'missing' })
  await first.declare(withIncarnation(first))
  await first.release()
  db.mutate('recordings', row => {
    row.ownerEpoch = '18446744073709551615'
    row.ownerState = 'live'
  })
  await expect(createWebRecordingAuthority(opts, 'id')).rejects.toThrow('reservation-epoch')
  expect(db.rows('recordings')[0]).toMatchObject({ ownerEpoch: '18446744073709551615', ownerState: 'live' })
})

test('crypto helpers generate random incarnation and SHA-256 lock names', async () => {
  const { webcrypto } = require('crypto')
  const { createWebRecordingIncarnation, webRecordingSha256 } = require('../../src/web/web-recording-authority')
  const first = createWebRecordingIncarnation(webcrypto),
    second = createWebRecordingIncarnation(webcrypto)
  expect(first).toMatch(/^[0-9a-f]{64}$/)
  expect(second).not.toBe(first)
  expect(await webRecordingSha256(webcrypto)('abc')).toBe(
    'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'
  )
})

test('a committed declaration remains observable when authority is lost before completion delivery', async () => {
  const db = transactionalDatabase()
  let fail
  const manager = { request(_name, _opts, callback) { callback({}); return new Promise((_resolve, reject) => { fail = reject }) } }
  const owner = (await createWebRecordingAuthority({ database: db, locks: manager, digest }, 'id')).authority
  db.pauseNextCompletion()
  const declaring = owner.declare(withIncarnation(owner))
  await db.requestsProcessed()
  fail(new Error('authority lost'))
  await Promise.resolve()
  db.complete()
  expect(await declaring).toMatchObject({ state: 'prepared', ownerEpoch: '1' })
  expect(owner.current()).toBe(false)
})

test('canceled policy selection permits a different coordinator when no acquisition ran', async () => {
  const db = transactionalDatabase(), controller = new AbortController()
  db.pauseNextCompletion()
  const pending = createWebRecordingAuthority({ database: db, locks: locks(), digest, signal: controller.signal }, 'id')
  await db.requestsProcessed()
  controller.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  db.complete()
  const previousWorker = global.SharedWorker
  global.SharedWorker = class {
    constructor() {
      this.onerror = null
      this.port = {
        onmessage: null, onmessageerror: null, start() {}, close() {},
        postMessage(message) { queueMicrotask(() => this.onmessage?.({ data: message })) }
      }
    }
  }
  try {
    const next = await createWebRecordingAuthority({ database: db, workerUrl: 'https://example.test/worker.js', digest }, 'id')
    expect(next.state).toBe('acquired')
    await next.authority.release()
  } finally {
    global.SharedWorker = previousWorker
  }
})

test('a lost completion is reconciled by a fresh overlapping transaction or reported unknown', async () => {
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const db = transactionalDatabase(), manager = locks()
    const owner = (await createWebRecordingAuthority({ database: db, locks: manager, digest }, 'id')).authority
    const original = db.transaction.bind(db)
    let drop = true
    const calls = []
    db.transaction = (names, mode, options) => {
      calls.push([names, mode])
      const tx = original(names, mode, options)
      if (drop && mode === 'readwrite' && names.includes('recordings')) {
        drop = false
        Object.defineProperty(tx, 'oncomplete', { set() {}, get() { return null } })
      }
      return tx
    }
    const pending = owner.declare(withIncarnation(owner))
    const outcome = pending.then(result => result.state, error => error.normalized?.code)
    await jest.advanceTimersByTimeAsync(12000)
    expect(await outcome).toBe('capability.unavailable')
    expect(calls.filter(([names, mode]) => mode === 'readwrite' &&
      ['recordings', 'authorityReservations', 'readerReceipts'].every(name => names.includes(name))).length).toBeGreaterThanOrEqual(2)
    expect(db.rows('recordings')).toHaveLength(1)
    expect(owner.current()).toBe(false)
    await owner.release()
    expect(db.rows('authorityReservations')[0].phase).toBe('released')
  } finally { jest.useRealTimers() }
})
