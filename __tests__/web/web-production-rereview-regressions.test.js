const { webcrypto } = require('crypto')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { recordingDatabase } = require('./web-authority-reservation-helper')

const digest = async () => 'ab'.repeat(32)
const locks = () => ({ request: (_name, _options, callback) => Promise.resolve(callback({})) })
const input = {
  id: 'rereview', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'source',
  context: { kind: 'ordered-recording', version: 1, peerId: 'p', connectionGeneration: 'c', databaseGeneration: 'd', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'x', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: 'notification', preference: null }] }
}
const flush = async () => { for (let i = 0; i < 100; i++) await Promise.resolve() }
const owner = () => new WebRecordingSourceOwner(new WebNotificationIngress(() => {}), [{ id: 'x', characteristic: {}, lease: { enable: async () => {}, release: async () => {} } }], 8, 1024, () => true)

test('failed barriers retain the realm bound while original acquisitions remain unresolved', async () => {
  const held = []
  try {
    for (let i = 0; i < 16; i++) {
      const db = recordingDatabase(), abort = new AbortController(), id = `rereview-${i}`
      const { authority } = await createWebRecordingAuthority({ database: db, locks: locks(), digest, crypto: webcrypto, signal: abort.signal }, id)
      db.pauseNext()
      const acquiring = authority.declare({ ...input, id, producerIncarnation: authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] })
      await db.requestsProcessed()
      const transaction = db.transaction.bind(db)
      let fail = true
      db.transaction = (...args) => { if (fail) { fail = false; throw new Error('barrier unavailable') }; return transaction(...args) }
      held.push({ db, authority })
      abort.abort()
      await expect(acquiring).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      await flush()
    }
    const db = recordingDatabase(), id = 'rereview-overflow'
    const { authority } = await createWebRecordingAuthority({ database: db, locks: locks(), digest, crypto: webcrypto }, id)
    const declaration = { ...input, id, producerIncarnation: authority.createProducerIncarnation(webcrypto), memberEpochs: [{ member: '0', epoch: '1' }] }
    await expect(authority.declare(declaration)).rejects.toMatchObject({ normalized: { operation: 'web-recording.reservation-limit' } })
    await authority.release()
  } finally {
    for (const item of held) item.db.complete()
    await flush()
    for (const item of held) { try { await item.authority.release() } catch { await item.authority.release() } }
  }
})

test('unstarted terminal commit is reported after authority loss even if retirement fails', async () => {
  const db = recordingDatabase()
  let lose
  const manager = { request: (_name, _options, callback) => { callback({}); return new Promise((_resolve, reject) => { lose = reject }) } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: manager, digest, crypto: webcrypto }, input, webcrypto)
  let opened, resume, committing
  const reached = new Promise(resolve => { opened = resolve }), delayed = new Promise(resolve => { resume = resolve })
  const startedCommit = new Promise(resolve => { committing = resolve })
  const open = bridge.open
  bridge.open = async (...args) => { await open(...args); opened(); await delayed }
  const finalize = bridge.finalizeUnstarted
  bridge.finalizeUnstarted = (...args) => { db.pauseNext(); committing(); return finalize(...args) }
  const supervisor = new WebOrderedRecordingSupervisor(owner(), bridge), abort = new AbortController()
  const start = supervisor.start(abort.signal)
  await reached; abort.abort(); await expect(start).rejects.toMatchObject({ name: 'AbortError' }); resume()
  await startedCommit; await db.requestsProcessed()
  lose(new Error('coordinator lost')); await flush(); db.complete(); await flush(); await supervisor.retry()
  expect(db.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  expect(supervisor.status()).toMatchObject({ state: 'failed', terminalCommitted: true })
})

test('an aggregate digest failure proves no coordinator requires cleanup', async () => {
  const db = recordingDatabase(), manager = locks()
  const request = jest.spyOn(manager, 'request')
  const cause = new AggregateError([new Error('digest failure')], 'digest failed')
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: manager, digest: async () => { throw cause }, crypto: webcrypto }, input, webcrypto)
  const supervisor = new WebOrderedRecordingSupervisor(owner(), bridge)
  await supervisor.start(); await supervisor.retry()
  expect(request).not.toHaveBeenCalled()
  expect(supervisor.status().setupFailure).toBe(cause)
  expect(supervisor.status().cleanupFailure).toBeNull()
  await expect(bridge.releaseAuthority()).resolves.toBeUndefined()
})

test('a failed coordinator release remains an explicit cleanup obligation', async () => {
  const db = recordingDatabase()
  db.transaction = () => { throw new AggregateError([new Error('status failed')], 'status failure') }
  const manager = { request: async (_name, _options, callback) => { await callback({}); throw new Error('release failed') } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: manager, digest, crypto: webcrypto }, input, webcrypto)
  await expect(bridge.prepare()).rejects.toThrow('Failed to release coordinator')
  await expect(bridge.releaseAuthority()).rejects.toMatchObject({ normalized: { operation: 'web-recording.authority-release-unproven' } })
})

test('an aggregate status failure with successful coordinator release leaves no cleanup obligation', async () => {
  const db = recordingDatabase(), cause = new AggregateError([new Error('status failed')], 'status failure')
  db.transaction = () => { throw cause }
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: locks(), digest, crypto: webcrypto }, input, webcrypto)
  await expect(bridge.prepare()).rejects.toBe(cause)
  await expect(bridge.releaseAuthority()).resolves.toBeUndefined()
})
