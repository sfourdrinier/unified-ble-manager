const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { createWebRecordingMarkerLedger } = require('../../src/web/web-recording-marker-ledger')
const { recordingDatabase } = require('./web-recovery-522-helper')

const op = n => n.toString(16).padStart(64, '0')
const context = {
  kind: 'ordered-recording',
  version: 1,
  peerId: 'peer',
  connectionGeneration: 'connection',
  databaseGeneration: 'graph',
  maxItems: 8,
  maxBytes: 1024,
  maxControls: 4,
  members: [
    {
      memberId: '0',
      publicMemberId: 'heart',
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
}
const declaration = {
  id: 'sample',
  maxBytes: 1048576,
  maxRecords: 20,
  context,
  producerIncarnation: 'incarnation',
  collectionId: '1',
  sourceGeneration: 'source',
  memberEpochs: [{ member: '0', epoch: '1' }]
}
async function setup() {
  const db = recordingDatabase()
  const { owner } = await createWebAuthorityReservations(db).acquire({
    declaration,
    ownerToken: 'a'.repeat(64),
    operationId: op(1),
    coordinator: { kind: 'locks', name: 'test' },
    expectedGeneration: null
  })
  const journal = createWebOrderedRecordingJournal(db, { ...declaration, ownerEpoch: owner.ownerEpoch }, owner)
  await journal.open(op(2))
  const ledger = createWebRecordingMarkerLedger(db, owner, declaration.producerIncarnation)
  return { db, ledger, recovery: createWebRecordingRecovery(db, { bound: (id, incarnation, after) => ({ id, incarnation, after }) }) }
}

test('inspection preserves an unsettled dispatched marker and reports unknown legacy inventory', async () => {
  const { ledger, recovery } = await setup()
  await ledger.reserve(op(3), {
    markerId: '0',
    target: { characteristicUuid: '2a37' },
    mode: 'with-response',
    bytes: Uint8Array.of(0, 255)
  })
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  await ledger.dispatchIntent(op(5), '0')
  const snapshot = await recovery.inspect('sample', 'incarnation')
  expect(snapshot.markerInventory).toBe('unknown')
  expect(snapshot.markers).toHaveLength(1)
  expect(snapshot.markers[0]).toMatchObject({ dispatchIntent: true, settlement: null, request: { bytes: [0, 255] } })
  expect(snapshot.uncertainty).toEqual({
    priorMarkerOutcome: 'unknown',
    sourceCleanup: 'unknown',
    oldOwnerActivity: 'possible'
  })
  await expect(recovery.inspect('sample', 'wrong')).rejects.toThrow()
})

test('receipt reconciliation follows a lost ledger transaction completion across contexts', async () => {
  const { db, ledger, recovery } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(1) })
  db.suppressNextCompletion()
  void ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  expect(await recovery.reconcileMarkerOperation('sample', 'incarnation', op(4))).toMatchObject({
    outcome: 'observed', signature: 'unverified',
    result: { admission: { state: 'admitted', ordinal: '5' } }
  })
  expect(await recovery.reconcileMarkerOperation('sample', 'incarnation', op(99))).toEqual({ outcome: 'unknown' })
})

test('inspection waits for an earlier held ledger transaction instead of guessing from delay', async () => {
  const { db, ledger, recovery } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(1) })
  await ledger.admit(op(4), '0', { state: 'admitted', ordinal: '5' })
  db.pauseNext()
  const write = ledger.dispatchIntent(op(5), '0')
  await db.requestsProcessed()
  let finished = false
  const read = recovery.inspect('sample', 'incarnation').then(value => {
    finished = true
    return value
  })
  await Promise.resolve()
  expect(finished).toBe(false)
  db.complete()
  await write
  expect((await read).markers[0]).toMatchObject({ dispatchIntent: true, settlement: null })
})

test('the web entrypoint exposes read-only inspection for the selected partition', async () => {
  const { db } = await setup()
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const partition = createWebRecordingPartition({
    database: db,
    crypto: { getRandomValues: bytes => bytes },
    digest: async () => 'x'
  })
  const previous = global.IDBKeyRange
  global.IDBKeyRange = { bound: lower => ({ id: lower[0], incarnation: lower[1] }) }
  try {
    const inspector = await createWebRecordingRecoveryInspector(partition)
    expect((await inspector.inspect('sample', 'incarnation')).markerInventory).toBe('unknown')
  } finally {
    global.IDBKeyRange = previous
  }
})

test('authority refuses reuse of an abandoned incarnation even when active keys are absent', async () => {
  const db = recordingDatabase()
  db.seed('abandonments', { recordingId: 'sample', producerIncarnation: 'incarnation', version: 1 })
  await expect(
    createWebAuthorityReservations(db).acquire({
      declaration,
      ownerToken: 'a'.repeat(64),
      operationId: op(1),
      coordinator: { kind: 'locks', name: 'test' },
      expectedGeneration: null
    })
  ).rejects.toThrow()
  expect(db.rows('recordings')).toHaveLength(0)
  expect(db.rows('authorityReservations')).toHaveLength(0)
})

test('a new incarnation can acquire an ID whose old incarnation is tombstoned', async () => {
  const db = recordingDatabase()
  db.seed('abandonments', { recordingId: 'sample', producerIncarnation: 'incarnation', version: 1 })
  const result = await createWebAuthorityReservations(db).acquire({
    declaration: { ...declaration, producerIncarnation: 'new-incarnation' },
    ownerToken: 'a'.repeat(64),
    operationId: op(1),
    coordinator: { kind: 'locks', name: 'test' },
    expectedGeneration: null
  })
  expect(result.state).toBe('acquired')
  expect(db.rows('abandonments')).toHaveLength(1)
})

test('acquisition observes a tombstone committed by an earlier overlapping transaction', async () => {
  const db = recordingDatabase()
  db.pauseNext()
  const tx = db.transaction(['abandonments'], 'readwrite')
  const committed = new Promise((resolve, reject) => {
    tx.oncomplete = resolve
    tx.onabort = reject
  })
  tx.objectStore('abandonments').put({ recordingId: 'sample', producerIncarnation: 'incarnation', version: 1 })
  await db.requestsProcessed()
  const acquire = createWebAuthorityReservations(db).acquire({
    declaration,
    ownerToken: 'a'.repeat(64),
    operationId: op(1),
    coordinator: { kind: 'locks', name: 'test' },
    expectedGeneration: null
  })
  db.complete()
  await committed
  await expect(acquire).rejects.toThrow()
  expect(db.rows('recordings')).toHaveLength(0)
})

test('the public recovery inspector pages marker evidence beyond the single-inspection limit', async () => {
  const { db } = await setup()
  for (let i = 0; i < 300; i++) {
    db.seed('markerLedger', {
      version: 1, recordingId: 'sample', producerIncarnation: 'incarnation', markerId: String(i),
      request: { bytes: [i % 256] }, dispatchIntent: i === 150, settlement: null
    })
  }
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const partition = createWebRecordingPartition({ database: db, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'x' })
  const previous = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, _upper, lowerOpen) => ({ id: lower[0], incarnation: lower[1], after: lowerOpen ? lower[2] : undefined }) }
  try {
    const inspector = await createWebRecordingRecoveryInspector(partition)
    let after = null
    const rows = []
    do {
      const page = await inspector.listMarkers('sample', 'incarnation', { after, limit: 17 })
      expect(page.markers.length).toBeLessThanOrEqual(17)
      rows.push(...page.markers)
      after = page.next
    } while (after !== null)
    expect(rows).toHaveLength(300)
    expect(new Set(rows.map(row => row.markerId)).size).toBe(300)
    expect(rows.find(row => row.markerId === '150')).toMatchObject({ dispatchIntent: true, settlement: null, request: { bytes: [150] } })
    await expect(inspector.listMarkers('sample', 'wrong', { after: null, limit: 17 })).rejects.toThrow()
  } finally {
    global.IDBKeyRange = previous
  }
})

test('public inspector reports operation evidence without claiming its signature was verified', async () => {
  const { db, ledger } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(1) })
  db.mutate('markerOperations', row => { row.signature = 'wrong' })
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const inspector = await createWebRecordingRecoveryInspector(createWebRecordingPartition({ database: db, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'x' }))
  expect(await inspector.reconcileMarkerOperation('sample', 'incarnation', op(3))).toMatchObject({ outcome: 'observed', signature: 'unverified' })
})

test('public recovery discovers active and cleared incarnations and their exact operation results', async () => {
  const { db, ledger } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: { path: 'x' }, mode: 'with-response', bytes: Uint8Array.of(0, 255) })
  await ledger.admit(op(4), '0', { state: 'not-admitted', evidence: 'stopped' })
  db.seed('readerReceipts', { recordingId: ':ubm-marker-retained-v1:["sample","old"]', version: 1, disposition: 'cleared-marker-evidence-retained', markerLedgerBytes: 42 })
  db.seed('readerReceipts', { recordingId: ':ubm-marker-retained-v1:["sample","older"]', version: 1, disposition: 'cleared-marker-evidence-retained', markerLedgerBytes: 21 })
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const previous = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
  try {
    const inspector = await createWebRecordingRecoveryInspector(createWebRecordingPartition({ database: db, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'x' }))
    const incarnations = await inspector.listIncarnations('sample', { after: null, limit: 1 })
    expect(incarnations.active).toBe('incarnation')
    expect(incarnations.retained).toEqual(['old'])
    expect((await inspector.listIncarnations('sample', { after: incarnations.next, limit: 1 })).retained).toEqual(['older'])
    const page = await inspector.listMarkerOperations('sample', 'incarnation', { after: null, limit: 1 })
    expect(page.operations).toHaveLength(1)
    expect(page.operations[0]).toMatchObject({ operationId: op(3), signature: 'unverified', transition: { kind: 'reserve' }, result: { request: { bytes: [0, 255] } } })
    const next = await inspector.listMarkerOperations('sample', 'incarnation', { after: page.next, limit: 1 })
    expect(next.operations[0]).toMatchObject({ operationId: op(4), transition: { kind: 'admit' }, result: { admission: { state: 'not-admitted', evidence: 'stopped' } } })
    expect(JSON.stringify(page)).not.toContain('a'.repeat(64))
  } finally { global.IDBKeyRange = previous }
})

test('incarnation pagination accepts an escaped maximum-length retained identity', async () => {
  const { db } = await setup()
  const longIncarnation = '\u0000'.repeat(256)
  for (const incarnation of [longIncarnation, 'later']) {
    db.seed('readerReceipts', { recordingId: `:ubm-marker-retained-v1:${JSON.stringify(['sample', incarnation])}`, version: 1, disposition: 'cleared-marker-evidence-retained', markerLedgerBytes: 1 })
  }
  const previous = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) }
  try {
    const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
    const inspector = await createWebRecordingRecoveryInspector(createWebRecordingPartition({ database: db, crypto: { getRandomValues: bytes => bytes }, digest: async () => 'x' }))
    const first = await inspector.listIncarnations('sample', { after: null, limit: 1 })
    expect(first.retained).toEqual([longIncarnation])
    expect(first.next.length).toBeGreaterThan(1024)
    expect((await inspector.listIncarnations('sample', { after: first.next, limit: 1 })).retained).toEqual(['later'])
  } finally { global.IDBKeyRange = previous }
})

test('explicit acknowledged disposition deletes only cleared evidence in bounded resumable batches', async () => {
  const { db, ledger, recovery } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(9) })
  await ledger.admit(op(4), '0', { state: 'not-admitted', evidence: 'stopped' })
  await expect(recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 1 })).rejects.toThrow()
  db.remove('recordings')
  db.seed('readerReceipts', { recordingId: ':ubm-marker-retained-v1:["sample","incarnation"]', version: 1, disposition: 'cleared-marker-evidence-retained', markerLedgerBytes: 42 })
  expect(() => recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: false, limit: 1 })).toThrow()
  expect(db.rows('markerOperations')).toHaveLength(2)
  expect(await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 1 })).toEqual({ state: 'more', deleted: 1 })
  expect(db.rows('markerLedger')).toHaveLength(1)
  expect(await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 1 })).toEqual({ state: 'more', deleted: 1 })
  expect(await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 1 })).toEqual({ state: 'more', deleted: 1 })
  expect(await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 1 })).toEqual({ state: 'disposed', deleted: 0 })
  expect(db.rows('markerLedger')).toHaveLength(0)
  expect(db.rows('markerOperations')).toHaveLength(0)
})

test('aborted disposition preserves all evidence and a retry deletes only the requested incarnation', async () => {
  const { db, ledger, recovery } = await setup()
  await ledger.reserve(op(3), { markerId: '0', target: {}, mode: 'with-response', bytes: Uint8Array.of(9) })
  db.remove('recordings')
  db.seed('readerReceipts', { recordingId: ':ubm-marker-retained-v1:["sample","incarnation"]', version: 1, disposition: 'cleared-marker-evidence-retained', markerLedgerBytes: 42 })
  db.seed('markerLedger', { version: 1, recordingId: 'sample', producerIncarnation: 'other', markerId: '0', request: { bytes: [7] } })
  db.abortNext(new Error('storage abort'))
  await expect(recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 2 })).rejects.toThrow('storage abort')
  expect(db.rows('markerOperations')).toHaveLength(1)
  expect(db.rows('markerLedger')).toHaveLength(2)
  await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 2 })
  await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 2 })
  await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 2 })
  expect(db.rows('markerLedger')).toEqual([expect.objectContaining({ producerIncarnation: 'other', request: { bytes: [7] } })])
  expect(await recovery.disposeRetained('sample', 'incarnation', { acknowledgeExport: true, limit: 2 })).toEqual({ state: 'absent', deleted: 0 })
})

test('public pages reflect mutations between independent transactions', async () => {
  const { db, recovery } = await setup()
  const seed = markerId => db.seed('markerLedger', { version: 1, recordingId: 'sample', producerIncarnation: 'incarnation', markerId, request: { bytes: [1] } })
  for (const id of ['1', '3', '5']) seed(id)
  const first = await recovery.listMarkers('sample', 'incarnation', { after: null, limit: 1 })
  seed('0'); seed('2'); seed('4')
  const second = await recovery.listMarkers('sample', 'incarnation', { after: first.next, limit: 2 })
  expect(first.markers.map(row => row.markerId)).toEqual(['1'])
  expect(second.markers.map(row => row.markerId)).toEqual(['2', '3'])
  expect(second.next).toBe('3')
})
