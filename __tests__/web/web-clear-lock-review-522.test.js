const { acquireWebRecordingClearLock } = require('../../src/web/web-recording-clear-lock')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')

test.each(['throw', 'reject'])('public clear classifies %s lock establishment failure at the lock boundary', async mode => {
  const failure = new DOMException('not allowed', 'SecurityError')
  const locks = { request() { if (mode === 'throw') throw failure; return Promise.reject(failure) } }
  const database = { transaction: jest.fn() }
  const controller = await createWebContinuationRecordingController({ database, locks })
  await expect(controller.clear('sample')).rejects.toMatchObject({
    code: 'capability.unsupported', platform: { domain: 'Web Locks', code: 'SecurityError' }
  })
  expect(database.transaction).not.toHaveBeenCalled()
})

test('a transient lock establishment error remains unavailable with its Web Locks reason', async () => {
  const locks = { request() { return Promise.reject(new DOMException('temporary', 'AbortError')) } }
  const controller = await createWebContinuationRecordingController({ database: { transaction: jest.fn() }, locks })
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable', platform: { domain: 'Web Locks', code: 'AbortError' } })
})

test('an acquired clear lock reports unexpected termination', async () => {
  let fail
  const locks = { request(_name, _options, callback) { callback({}); return new Promise((_resolve, reject) => { fail = reject }) } }
  const lease = await acquireWebRecordingClearLock(locks)
  const error = new Error('stolen')
  fail(error)
  await expect(lease.ended).resolves.toMatchObject({ state: 'lost', error })
})

test('loss during storage readiness fences a later allocation dispatch', async () => {
  let lose, resume
  const locks = { request(_name, _options, callback) { callback({}); return new Promise((_resolve, reject) => { lose = reject }) } }
  const database = { ready: () => new Promise(resolve => { resume = resolve }), transaction: jest.fn() }
  const controller = await createWebContinuationRecordingController({ database, locks })
  const clear = controller.clear('sample').catch(error => error)
  for (let i = 0; i < 20 && !resume; i++) await Promise.resolve()
  expect(resume).toBeDefined()
  lose(new Error('stolen'))
  await Promise.resolve()
  resume()
  expect(await clear).toMatchObject({ code: 'capability.unavailable' })
  expect(database.transaction).not.toHaveBeenCalled()
})

test('clear receipts have bounded pages without a fixed total capacity and can be acknowledged exactly', async () => {
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = recordingDatabase()
  const oldRange = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen = false) => ({ lower, upper, lowerOpen }) }
  try {
    for (let i = 1; i <= 70; i++) {
      db.seed('readerReceipts', {
        recordingId: `:ubm-clear-v4:${String(i).padStart(20, '0')}`, version: 3,
        targetRecordingId: 'sample', operationId: `g${i}`, records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1'
      })
    }
    db.seed('readerReceipts', { recordingId: ':ubm-clear-v4:0000000000000000000X' })
    const recovery = createWebRecordingRecovery(db, { retained: (id, after) => ({ lower: after || `:ubm-marker-retained-v1:["${id}",`, upper: `:ubm-marker-retained-v1:["${id}",\uffff`, lowerOpen: !!after }) })
    let after = null, receipts = [], malformed = []
    do {
      const page = await recovery.listIncarnations('sample', { after: null, limit: 1, includeClearReceipts: true, clearReceiptsAfter: after })
      receipts.push(...page.clearReceipts.filter(receipt => receipt.version === 3))
      malformed.push(...page.malformedClearReceipts)
      expect(page).toHaveProperty('clearReceiptsNext')
      after = page.clearReceiptsNext
    } while (after !== null)
    expect(receipts).toHaveLength(70)
    expect(malformed).toContainEqual({ recordingId: ':ubm-clear-v4:0000000000000000000X', error: 'malformed-clear-receipt' })
    const receipt = receipts[0]
    expect(await recovery.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).toEqual({ state: 'more', deleted: 1 })
    expect(await recovery.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).toEqual({ state: 'absent', deleted: 0 })
  } finally { global.IDBKeyRange = oldRange }
})

test('explicit receipt acknowledgement settles independently when another clear lock is stolen', async () => {
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = recordingDatabase()
  const receipt = { recordingId: ':ubm-clear-v4:00000000000000000001', version: 3, targetRecordingId: 'sample', operationId: 'g1', records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1' }
  db.seed('readerReceipts', receipt)
  let current
  const locks = { request(_name, _options, callback) {
    if (current) return Promise.resolve(callback(null))
    let reject
    const stolen = new Promise((_resolve, fail) => { reject = fail })
    const entry = { steal() { current = undefined; reject(new Error('stolen')) } }
    current = entry
    return Promise.race([callback({}), stolen]).finally(() => { if (current === entry) current = undefined })
  } }
  const first = await acquireWebRecordingClearLock(locks)
  const recovery = createWebRecordingRecovery(db)
  db.pauseNext()
  const acknowledgement = recovery.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })
  await db.requestsProcessed()
  current.steal()
  expect((await first.ended).state).toBe('lost')
  const second = await acquireWebRecordingClearLock(locks)
  db.complete()
  expect(await acknowledgement).toEqual({ state: 'more', deleted: 1 })
  expect(db.rows('readerReceipts').some(row => row.recordingId === receipt.recordingId)).toBe(false)
  second()
  expect((await second.ended).state).toBe('released')
})

test('retiring an exact receipt preserves independently inspectable proof across new inspectors', async () => {
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = recordingDatabase()
  const receipt = { recordingId: ':ubm-clear-v4:00000000000000000001', version: 3, targetRecordingId: 'sample', operationId: 'g1', records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1' }
  db.seed('readerReceipts', receipt)
  const first = createWebRecordingRecovery(db)
  expect(await first.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).toEqual({ state: 'more', deleted: 1 })
  const afterReload = createWebRecordingRecovery({ transaction: (...args) => db.transaction(...args) })
  const verify = clearReceipt => afterReload.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt, verifyRetiredClearReceipt: true })
  expect(await verify(receipt)).toEqual({ state: 'more', deleted: 0, retiredReceipt: receipt })
  expect(await afterReload.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).toEqual({ state: 'absent', deleted: 0 })
  await expect(verify({ ...receipt, ownerEpoch: '2' })).rejects.toThrow()
})

test('an archive conflict never removes the original receipt', async () => {
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = recordingDatabase()
  const receipt = { recordingId: ':ubm-clear-v4:00000000000000000001', version: 3, targetRecordingId: 'sample', operationId: 'g1', records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1' }
  db.seed('readerReceipts', receipt)
  db.seed('readerReceipts', { recordingId: ':ubm-clear-retired-v1:[":ubm-clear-v4:00000000000000000001","g1"]', invalid: true })
  const inspector = createWebRecordingRecovery(db)
  await expect(inspector.disposeRetained('sample', 'producer', { acknowledgeExport: true, limit: 1, clearReceipt: receipt })).rejects.toThrow()
  expect(db.rows('readerReceipts')).toContainEqual(receipt)
})

test('an aborted retirement leaves the original proof and no archive', async () => {
  const { recordingDatabase } = require('./web-recovery-522-helper')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const db = recordingDatabase()
  const receipt = { recordingId: ':ubm-clear-v4:00000000000000000001', version: 3, targetRecordingId: 'sample', operationId: 'g1', records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1' }
  db.seed('readerReceipts', receipt)
  const inspector = createWebRecordingRecovery(db)
  const options = { acknowledgeExport: true, limit: 1, clearReceipt: receipt }
  db.abortNext(new Error('storage abort'))
  await expect(inspector.disposeRetained('sample', 'producer', options)).rejects.toThrow()
  expect(db.rows('readerReceipts')).toContainEqual(receipt)
  expect(await inspector.disposeRetained('sample', 'producer', { ...options, verifyRetiredClearReceipt: true })).toEqual({ state: 'absent', deleted: 0 })
  expect(await inspector.disposeRetained('sample', 'producer', options)).toEqual({ state: 'more', deleted: 1 })
})

test('public partition controllers coordinate by journal identity across independent connections', async () => {
  const { createWebRecordingPartition } = require('../../src/web')
  const { webcrypto } = require('crypto')
  const held = new Set(), names = []
  const locks = { request(name, _options, callback) {
    names.push(name)
    if (held.has(name)) return Promise.resolve(callback(null))
    held.add(name)
    return Promise.resolve(callback({})).finally(() => held.delete(name))
  } }
  const resume = {}
  const database = name => ({ name, ready: () => new Promise(resolve => { resume[name] = resolve }), transaction: jest.fn() })
  const partition = db => createWebRecordingPartition({ database: db, locks, crypto: webcrypto, digest: async () => 'unused' })
  const a = await partition(database('alpha')).createController()
  const same = await partition(database('alpha')).createController()
  const other = await partition(database('beta')).createController()
  const first = a.clear('sample').catch(error => error)
  for (let i = 0; i < 30 && !resume.alpha; i++) await Promise.resolve()
  expect(resume.alpha).toBeDefined()
  await expect(same.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  const second = other.clear('sample').catch(error => error)
  for (let i = 0; i < 30 && !resume.beta; i++) await Promise.resolve()
  expect(names).toHaveLength(3)
  expect(names[0]).toBe(names[1])
  expect(names[2]).not.toBe(names[0])
  resume.alpha()
  resume.beta()
  await Promise.all([first, second])
})

test('a named database cannot be assigned a conflicting journal identity', async () => {
  const { createWebRecordingPartition } = require('../../src/web')
  const { webcrypto } = require('crypto')
  const locks = { request: jest.fn() }
  const partition = createWebRecordingPartition({
    database: { name: 'journal-a', transaction: jest.fn() },
    journalIdentity: 'journal-b', locks, crypto: webcrypto, digest: async () => 'unused'
  })
  const controller = await partition.createController()
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'argument.invalid' })
  expect(locks.request).not.toHaveBeenCalled()
})

test('a pending lock request does not block a different journal', async () => {
  const names = []
  const locks = { request(name, _options, callback) {
    names.push(name)
    if (names.length === 1) return new Promise(() => {})
    return Promise.resolve(callback({}))
  } }
  jest.useFakeTimers()
  const pending = acquireWebRecordingClearLock(locks, undefined, 'first').catch(() => {})
  const lease = await acquireWebRecordingClearLock(locks, undefined, 'second')
  expect(names).toHaveLength(2)
  expect(names[0]).not.toBe(names[1])
  lease()
  expect(await lease.ended).toEqual({ state: 'released' })
  await jest.advanceTimersByTimeAsync(5001)
  await pending
  jest.useRealTimers()
})
