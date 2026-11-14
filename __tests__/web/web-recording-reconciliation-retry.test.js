const { createWebRecordingJournalBridge } = require('../../src/web/web-recording-journal-bridge')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { recordingDatabase, reserveJournal } = require('./web-continuation-recording-controller-helper')

const declaration = {
  id: 'retry', maxBytes: 1048576, maxRecords: 10, producerIncarnation: 'producer',
  collectionId: '1', sourceGeneration: 'generation', ownerEpoch: '1',
  context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 4,
    members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }] }
}

function bridgeFor(db, wrap) {
  let journal
  return createWebRecordingJournalBridge(db, async memberEpochs => {
    journal = await reserveJournal(db, { ...declaration, memberEpochs })
    return declaration
  }, async () => ({ current: () => true, release: async () => {} }), () => wrap(journal))
}

test('reconciles a settled ambiguous commit on retry before acknowledging the retained head', async () => {
  const db = recordingDatabase()
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  let source, wake, commits = 0, reconciliations = 0
  const bridge = bridgeFor(db, journal => {
    return { ...journal,
      commitWithDurability: async delivery => { commits++; const receipt = await journal.commitWithDurability(delivery); if (delivery.id === '1') throw new Error('lost reply'); return receipt },
      reconcile: async delivery => { reconciliations++; if (reconciliations === 1) return { outcome: 'unknown', durability: 'default' }; return journal.reconcile(delivery) }
    }
  })
  const owner = { reserve: () => ({ memberships: () => [{ member: '0', epoch: '1' }] }), isCurrent: () => true, release: async () => {}, install: callback => {
    wake = callback
    source = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
    return { source, leases: [{ id: 'heart', enable: async () => {}, release: async () => {} }] }
  } }
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  await supervisor.start()
  ingress.admit(characteristic, Uint8Array.from([9])); wake()
  for (let i = 0; i < 100 && !supervisor.status().runtimeFailure; i++) await Promise.resolve()
  expect(source.peek().id).toBe('1')
  expect(db.rows('records')).toHaveLength(1)
  expect(commits).toBe(1)
  await supervisor.retry()
  expect(supervisor.status().state).toBe('finished')
  expect(db.rows('records').map(row => row.record.kind)).toEqual(['value', 'terminal'])
  expect(commits).toBe(2)
  expect(bridge.lastCommit()).toMatchObject({ evidence: 'commit' })
})

test('orders reconciliation behind an overlapping unsettled write before acknowledging', async () => {
  const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
  const { WebRetainedRecordingPump } = require('../../src/core/web-retained-recording-pump')
  const db = transactionalDatabase()
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const source = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
  let write
  const bridge = bridgeFor(db, journal => {
    return { ...journal, commitWithDurability: delivery => {
      write = journal.commitWithDurability(delivery)
      return Promise.reject(new Error('lost reply'))
    } }
  })
  await bridge.prepare(); await bridge.open(source)
  ingress.admit(characteristic, Uint8Array.from([4]))
  db.pauseNextCompletion()
  const pump = new WebRetainedRecordingPump(source, bridge)
  const pending = pump.runOnce()
  await db.requestsProcessed()
  expect(source.peek().id).toBe('1')
  expect(db.rows('records')).toHaveLength(0)
  db.complete()
  await write
  await pending
  expect(source.peek()).toBeNull()
  expect(db.rows('records')).toHaveLength(1)
  expect(bridge.lastCommit()).toMatchObject({ evidence: 'reconciled', durability: null, reconciliationDurability: 'strict' })
})

test('waits for a late rejected operation to settle before retrying its exact identity', async () => {
  jest.useFakeTimers()
  try {
    const db = recordingDatabase()
    const ingress = new WebNotificationIngress()
    const characteristic = {}
    let source, wake, rejectReply, writes = 0, reads = 0
    const reply = new Promise((_, reject) => { rejectReply = reject })
    const bridge = bridgeFor(db, journal => {
      return { ...journal,
        commitWithDurability: async delivery => { writes++; const receipt = await journal.commitWithDurability(delivery); if (delivery.id === '1') return reply; return receipt },
        reconcile: async delivery => { reads++; if (reads === 1) return { outcome: 'unknown', durability: 'default' }; return journal.reconcile(delivery) }
      }
    })
    const owner = { reserve: () => ({ memberships: () => [{ member: '0', epoch: '1' }] }), isCurrent: () => true, release: async () => {}, install: callback => {
      wake = callback
      source = ingress.registerRetained(new Map([[characteristic, '0']]), 8, 1024, { membership: true })
      return { source, leases: [{ id: 'heart', enable: async () => {}, release: async () => {} }] }
    } }
    const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
    await supervisor.start()
    ingress.admit(characteristic, Uint8Array.from([3])); wake()
    for (let i = 0; i < 100 && db.rows('records').length === 0; i++) await Promise.resolve()
    jest.advanceTimersByTime(30000)
    for (let i = 0; i < 100 && !supervisor.status().runtimeFailure; i++) await Promise.resolve()
    await supervisor.retry()
    expect(writes).toBe(1)
    expect(source.peek().id).toBe('1')
    rejectReply(new Error('late lost reply'))
    for (let i = 0; i < 100 && reads === 0; i++) await Promise.resolve()
    for (let i = 0; i < 30; i++) await Promise.resolve()
    await supervisor.retry()
    expect(supervisor.status().state).toBe('finished')
    expect(writes).toBe(2)
    expect(db.rows('records').map(row => row.record.kind)).toEqual(['value', 'terminal'])
  } finally {
    jest.useRealTimers()
  }
})
