const { createWebOrderedRecordingJournal } = require('../../src/web/web-ordered-recording-journal')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { MAX_RECORDING_BATCH_BYTES } = require('../../src/backend-contract/continuation-recording-bounds')
const { parseOrderedRecordingBatch } = require('../../src/core/ordered-recording')

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
  maxRecords: 2,
  context,
  producerIncarnation: 'producer',
  collectionId: '1',
  sourceGeneration: 'generation',
  ownerEpoch: '1',
  memberEpochs: [{ member: '0', epoch: '1' }]
}
const value = (bytes = [0, 255], ordinal = '18446744073709551615') => ({
  kind: 'value',
  ordinal,
  member: '0',
  membership: { member: '0', epoch: '1' },
  bytes: Uint8Array.from(bytes),
  delivery: 'notification',
  provenance: 'read-or-notification'
})

async function journalFor(db, input) {
  const { ownerEpoch: _epoch, ...withoutEpoch } = input
  const { owner } = await createWebAuthorityReservations(db).acquire({
    declaration: withoutEpoch,
    ownerToken: 'a'.repeat(64),
    operationId: 'b'.repeat(64),
    coordinator: { kind: 'locks', name: 'test' },
    expectedGeneration: null
  })
  const journal = createWebOrderedRecordingJournal(db, { ...input, ownerEpoch: owner.ownerEpoch }, owner)
  let cleanup = false
  return {
    ...journal,
    open: () => journal.open('c'.repeat(64)),
    seal: async () => {
      if (!cleanup) {
        await createWebAuthorityReservations(db).cleanupPending(owner, 'd'.repeat(64))
        cleanup = true
      }
      return journal.seal()
    }
  }
}

async function prepared(db) {
  const journal = await journalFor(db, declaration)
  await journal.declare()
  await journal.open()
  return journal
}

test('v2 commits and reconciles exact failure observations with ordered metadata', async () => {
  const db = transactionalDatabase()
  const journal = await journalFor(db, { ...declaration, context: { ...context, version: 2 } })
  await journal.declare()
  await journal.open()
  const entries = [
    { key: 'same', value: { kind: 'text', value: 'first' } },
    { key: 'same', value: { kind: 'int', value: '2' } }
  ]
  const failure = { code: 'platform.failure', domain: 'stream', operation: 'observe', detail: null,
    platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {}, platformMetadataEntries: entries }
  const record = { kind: 'failure-observation', failure, sourceRejection: null, sourceFailureMember: null,
    rejectedLoss: null, tailUncertain: false }
  db.pauseNextCompletion()
  let settled = false
  const commit = journal.commit({ id: '1', record }).then(value => { settled = true; return value })
  await db.requestsProcessed()
  expect(settled).toBe(false)
  db.complete()
  expect(await commit).toBe(1)
  expect((await journal.reconcile({ id: '1', record })).outcome).toBe('committed')
  expect(db.rows('records')[0]).toMatchObject({ metadata: { version: 2 }, record })
  await journal.fail(failure)
  expect(db.rows('recordings')[0].failure.platformMetadataEntries).toEqual(entries)
})

test('v2 rejects malformed and oversized failure metadata before opening a transaction', async () => {
  const db = transactionalDatabase()
  const journal = await journalFor(db, { ...declaration, context: { ...context, version: 2 } })
  await journal.declare()
  await journal.open()
  const failure = { code: 'platform.failure', domain: 'stream', operation: 'observe', detail: null,
    platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} }
  const invalidEntries = [
    [{ key: 'x', value: { kind: 'int', value: '-0' } }],
    [{ key: 'x', value: { kind: 'text', value: 'x'.repeat(65537) } }]
  ]
  for (const platformMetadataEntries of invalidEntries) {
    const before = db.modes.length
    await expect(journal.fail({ ...failure, platformMetadataEntries })).rejects.toThrow()
    expect(db.modes).toHaveLength(before)
  }
  const before = db.modes.length
  await expect(journal.fail({ ...failure, platformMetadata: { label: { kind: 'text', value: 'é'.repeat(32769) } } })).rejects.toThrow()
  expect(db.modes).toHaveLength(before)
  expect(db.rows('recordings')[0].failure).toBeNull()
})

test('v2 terminal and failed stop survive retirement and reader acknowledgement', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await journalFor(db, { ...declaration, context: { ...context, version: 2 } })
  await journal.declare()
  await journal.open()
  const entries = [{ key: 'x', value: { kind: 'text', value: 'one' } }, { key: 'x', value: { kind: 'text', value: 'two' } }]
  const failure = { code: 'stream.failed', domain: 'stream', operation: 'notify', detail: null,
    platformDomain: 'web', platformCode: null, platformMessage: null, platformMetadata: {}, platformMetadataEntries: entries }
  const terminal = { kind: 'terminal', reason: { kind: 'source-failed', failure }, tailUncertain: true }
  const outcome = { version: 2, state: 'failed', error: { kind: 'source', source: failure } }
  await journal.commit({ id: '1', record: terminal, outcome })
  await journal.retire('1')
  await journal.seal()
  const controller = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 10, maxBytes: 10000 })
  expect(batch.records[0]).toMatchObject({ metadata: { version: 2 }, record: terminal })
  await controller.acknowledge('sample', batch.token)
  expect(await controller.stopOrdered('sample')).toMatchObject({ recording: outcome, hostCleanup: { state: 'uncertain' } })
})

test('the v1 writer rejects v2-only failure metadata before opening a transaction', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const before = db.modes.length
  const failure = {
    code: 'platform.failure', domain: 'stream', operation: 'observe', detail: null,
    platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {},
    platformMetadataEntries: [{ key: 'duplicate', value: { kind: 'text', value: 'value' } }]
  }
  await expect(journal.fail(failure)).rejects.toThrow()
  expect(db.modes).toHaveLength(before)
  expect(db.rows('recordings')[0].failure).toBeNull()
})

test('commits exact bytes after transaction completion and retries only an identical payload', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  db.pauseNextCompletion()
  const bytes = value()
  let settled = false
  const result = journal.commit({ id: '1', record: bytes }).then(x => {
    settled = true
    return x
  })
  await db.requestsProcessed()
  expect(settled).toBe(false)
  db.complete()
  expect(await result).toBe(1)
  bytes.bytes[0] = 7
  expect(db.rows('records')[0].record.bytes).toEqual([0, 255])
  expect(await journal.commit({ id: '1', record: value() })).toBe(1)
  await expect(journal.commit({ id: '1', record: value([0, 1]) })).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(1)
})

test('quota never evicts pending identities, and retirement is explicit', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  await journal.commit({ id: '1', record: value([], '1') })
  await journal.commit({ id: '2', record: value([1], '2') })
  await expect(journal.commit({ id: '3', record: value([2], '3') })).rejects.toThrow()
  expect(db.rows('pendingIdentities')).toHaveLength(2)
  await journal.retire('1')
  expect(db.rows('pendingIdentities')).toHaveLength(2)
  expect(db.rows('pendingIdentities').find(row => row.deliveryId === '1').retired).toBe(true)
  expect(db.rows('recordings')[0].pendingCount).toBe(1)
})

test('stale owner epoch and transaction abort cannot append', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  db.mutate('recordings', row => {
    row.ownerEpoch = '2'
  })
  await expect(journal.commit({ id: '1', record: value() })).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(0)
  db.mutate('recordings', row => {
    row.ownerEpoch = '1'
  })
  const error = new Error('disk full')
  db.abortNext(error)
  await expect(journal.commit({ id: '1', record: value() })).rejects.toBe(error)
  expect(db.rows('records')).toHaveLength(0)
})

test('rejects a value from a stale member epoch without changing the journal', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const record = value()
  record.membership.epoch = '2'
  await expect(journal.commit({ id: '1', record })).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(0)
})

test('does not seal until a committed terminal has been acknowledged and retired', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  await journal.commit({ id: '1', record: terminal })
  await expect(journal.seal()).rejects.toThrow()
  await journal.retire('1')
  await journal.seal()
  expect(db.rows('recordings')[0].ownerState).toBe('sealed')
})

test('persists structured failure only when the transaction commits', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const failure = {
    code: 'storage',
    domain: 'web',
    operation: 'write',
    detail: 'failed',
    platformDomain: null,
    platformCode: null,
    platformMessage: null,
    platformMetadata: {}
  }
  const error = new Error('write denied')
  db.abortNext(error)
  await expect(journal.fail(failure)).rejects.toBe(error)
  expect(db.rows('recordings')[0].failure).toBeNull()
  await journal.fail(failure)
  expect(db.rows('recordings')[0].failure).toEqual(failure)
})

test('a prepared journal retains pre-source failure without inventing a source record', async () => {
  const db = transactionalDatabase()
  const journal = await journalFor(db, declaration)
  const failure = {
    code: 'storage',
    domain: 'web',
    operation: 'prepare',
    detail: 'failed',
    platformDomain: null,
    platformCode: null,
    platformMessage: null,
    platformMetadata: {}
  }
  await journal.declare()
  await journal.fail(failure)
  expect(db.rows('recordings')[0]).toMatchObject({ ownerState: 'prepared', failure })
  expect(db.rows('records')).toHaveLength(0)
  await journal.open()
  await expect(
    journal.commit({
      id: '1',
      record: {
        kind: 'setup-terminal',
        sourceCreated: false,
        tailUncertain: false,
        failure,
        setupFailure: null,
        fenceFailure: null,
        members: []
      }
    })
  ).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(0)
})

test('writes the last safe journal ordinal once, then reports exhaustion', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  db.mutate('recordings', row => {
    row.nextOrdinal = Number.MAX_SAFE_INTEGER
    row.readerCursor = Number.MAX_SAFE_INTEGER - 1
    row.lastDeliveryId = '0'
    row.readerIntegrity.deliveryId = '0'
  })
  expect(await journal.commit({ id: '1', record: value([], '1') })).toBe(Number.MAX_SAFE_INTEGER)
  await expect(journal.commit({ id: '2', record: value([], '2') })).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(1)
})

test('only retries an exact previously retired identity', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  await journal.commit({ id: '2', record: value([], '1') })
  await expect(journal.retire('1')).rejects.toThrow()
  expect(db.rows('pendingIdentities')).toHaveLength(1)
  await journal.retire('2')
  await journal.retire('2')
})

test('enforces the serialized byte budget as well as the record budget', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  await expect(journal.commit({ id: '1', record: value(new Array(400000).fill(255), '1') })).rejects.toMatchObject({
    normalized: { code: 'stream.quota' }
  })
  expect(db.rows('records')).toHaveLength(0)
})

test('refuses corrupted cursor and quota accounting without changing records', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  db.mutate('recordings', row => {
    row.readerCursor = 5
  })
  await expect(journal.commit({ id: '1', record: value([], '1') })).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(0)
})

test.each([
  ['missing row', db => db.remove('records')],
  [
    'payload bytes',
    db =>
      db.mutate('records', row => {
        row.record.bytes = [99]
      })
  ],
  [
    'payload metadata',
    db =>
      db.mutate('records', row => {
        row.record.provenance = 'unknown'
      })
  ],
  [
    'envelope metadata',
    db =>
      db.mutate('records', row => {
        row.metadata.peerId = 'other'
      })
  ],
  [
    'row version',
    db =>
      db.mutate('records', row => {
        row.version = 2
      })
  ],
  [
    'row ordinal',
    db =>
      db.mutate('records', row => {
        row.ordinal = 2
      })
  ],
  [
    'row recording identity',
    db =>
      db.mutate('records', row => {
        row.recordingId = 'other'
      })
  ],
  [
    'unexpected metadata',
    db =>
      db.mutate('records', row => {
        row.metadata.extra = 'unexpected'
      })
  ],
  [
    'byte accounting',
    db =>
      db.mutate('records', row => {
        row.byteLength++
      })
  ],
  [
    'uncommitted ordinal',
    db =>
      db.mutate('pendingIdentities', row => {
        row.ordinal = 2
      })
  ]
])('duplicate and reconciliation reject %s', async (_name, corrupt) => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const delivery = { id: '1', record: value([1, 2], '1') }
  await journal.commit(delivery)
  corrupt(db)
  await expect(journal.commit(delivery)).rejects.toThrow()
  await expect(journal.reconcile(delivery)).rejects.toThrow()
})

test('reconciliation is read-only and distinguishes absent from no longer provable', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const delivery = { id: '1', record: value([1], '1') }
  expect(await journal.reconcile(delivery)).toEqual({ outcome: 'absent', durability: 'strict' })
  expect(db.modes.at(-1)).toBe('readonly')
  expect(db.rows('records')).toHaveLength(0)
  await journal.commit(delivery)
  await journal.retire('1')
  expect(await journal.reconcile(delivery)).toEqual({ outcome: 'unknown', durability: 'strict' })
})

test('a later reconciliation waits for an earlier unresolved write transaction', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const delivery = { id: '1', record: value([1], '1') }
  db.pauseNextCompletion()
  const commit = journal.commit(delivery)
  await db.requestsProcessed()
  let settled = false
  const reconciliation = journal.reconcile(delivery).then(result => {
    settled = true
    return result
  })
  await new Promise(resolve => setImmediate(resolve))
  expect(settled).toBe(false)
  db.complete()
  expect(await commit).toBe(1)
  expect(await reconciliation).toEqual({ outcome: 'committed', ordinal: 1, durability: 'strict' })
})

test('a later reconciliation reports absence after the earlier write aborts', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const delivery = { id: '1', record: value([1], '1') }
  const error = new Error('aborted')
  db.abortNext(error)
  db.pauseNextCompletion()
  const commit = journal.commit(delivery)
  await db.requestsProcessed()
  const reconciliation = journal.reconcile(delivery)
  db.complete()
  await expect(commit).rejects.toBe(error)
  expect(await reconciliation).toEqual({ outcome: 'absent', durability: 'strict' })
})

test('reports the requested durability for the specific commit and fallback', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  expect(await journal.commitWithDurability({ id: '1', record: value([], '1') })).toEqual({
    ordinal: 1,
    durability: 'strict'
  })
  db.fallbackDurability = true
  expect(await journal.commitWithDurability({ id: '2', record: value([], '2') })).toEqual({
    ordinal: 2,
    durability: 'default'
  })
  expect(await journal.reconcile({ id: '2', record: value([], '2') })).toEqual({
    outcome: 'committed',
    ordinal: 2,
    durability: 'default'
  })
})

test('admits the maximum readable serialized envelope and rejects one byte more', async () => {
  const db = transactionalDatabase()
  const journal = await journalFor(db, { ...declaration, maxBytes: 10_000_000 })
  await journal.declare()
  await journal.open()
  // Set the next ordinal to its longest representation; the size check must use
  // the actual ordinal, not a one-digit validation placeholder.
  db.mutate('recordings', row => {
    row.nextOrdinal = Number.MAX_SAFE_INTEGER
    row.readerCursor = Number.MAX_SAFE_INTEGER - 1
    row.lastDeliveryId = '0'
    row.readerIntegrity.deliveryId = '0'
  })
  const empty = value([], '1')
  const metadata = {
    kind: 'ordered',
    version: 1,
    recordingId: declaration.id,
    peerId: context.peerId,
    collectionId: '1',
    sourceGeneration: { kind: 'opaque', value: 'generation' },
    deliveryId: '1'
  }
  const base = Buffer.byteLength(
    JSON.stringify({ ordinal: Number.MAX_SAFE_INTEGER, metadata, record: { ...empty, bytes: [] } })
  )
  const extra = MAX_RECORDING_BATCH_BYTES - base
  const count = Math.floor((extra + 1) / 2)
  const bytes = new Uint8Array(count)
  if (extra % 2 === 0) bytes[0] = 10
  const record = { ...empty, bytes }
  expect(await journal.commit({ id: '1', record })).toBe(Number.MAX_SAFE_INTEGER)
  const stored = db.rows('records')[0]
  expect(stored.byteLength).toBe(MAX_RECORDING_BATCH_BYTES)
  expect(
    parseOrderedRecordingBatch(
      {
        token: 'reader',
        records: [{ ordinal: stored.ordinal, metadata: stored.metadata, record: stored.record }],
        bytes: stored.byteLength,
        more: false
      },
      declaration.id,
      { maxItems: 1, maxBytes: MAX_RECORDING_BATCH_BYTES }
    ).records
  ).toHaveLength(1)

  const db2 = transactionalDatabase()
  const journal2 = await journalFor(db2, { ...declaration, maxBytes: 10_000_000 })
  await journal2.declare()
  await journal2.open()
  db2.mutate('recordings', row => {
    row.nextOrdinal = Number.MAX_SAFE_INTEGER
    row.readerCursor = Number.MAX_SAFE_INTEGER - 1
    row.lastDeliveryId = '0'
    row.readerIntegrity.deliveryId = '0'
  })
  bytes[1] = 10
  await expect(journal2.commit({ id: '1', record })).rejects.toThrow()
  expect(db2.rows('records')).toHaveLength(0)
}, 60000)

test('retains exact terminal outcome after source retirement and reader acknowledgement', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const source = {
    code: 'stream.failed',
    domain: 'stream',
    operation: 'notify',
    detail: 'radio',
    platformDomain: 'web',
    platformCode: 'x',
    platformMessage: null,
    platformMetadata: {}
  }
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'source-failed', failure: source },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  const outcome = { version: 1, state: 'failed', error: { kind: 'source', source } }
  const status = {
    recordingId: 'sample',
    phase: 'stopped',
    accepting: false,
    records: 1,
    bytes: 1,
    lostRecords: 0,
    maxBytes: declaration.maxBytes,
    maxRecords: declaration.maxRecords,
    encrypted: false,
    orderedOwner: {
      version: 1,
      producerIncarnation: 'producer',
      collectionId: '1',
      sourceGeneration: { kind: 'opaque', value: 'generation' },
      state: 'sealed',
      uncertainTail: null
    },
    runtimeFailure: null,
    collectionFailure: null
  }
  const delivery = { id: '1', record: terminal, outcome, status }
  await journal.commit(delivery)
  await journal.retire('1')
  await journal.seal()
  const controller = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 2, maxBytes: 10000 })
  await controller.acknowledge('sample', batch.token)
  expect(db.rows('records')).toHaveLength(0)
  expect(db.rows('pendingIdentities')).toHaveLength(0)
  expect(await controller.stopOrdered('sample')).toEqual({ recording: outcome, hostCleanup: { state: 'uncertain' } })
  expect(await controller.status('sample')).toMatchObject({ phase: 'stopped', accepting: false, records: 0 })
  expect(await journal.reconcile(delivery)).toMatchObject({ outcome: 'committed', ordinal: 1 })
  await expect(
    journal.reconcile({ ...delivery, outcome: { version: 1, state: 'finished', error: null } })
  ).rejects.toThrow()
})

test('retains an explicitly observed later stop and cleanup after terminal record deletion', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = { kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null, sourceFailureMember: null,
    secondaryFailure: null, additionalFailureCount: '0' }
  const delivery = { id: '1', record: terminal }
  await journal.commit(delivery)
  await journal.retire('1')
  await journal.seal()
  const controller = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 2, maxBytes: 10000 })
  await controller.acknowledge('sample', batch.token)
  const result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } }
  const observation = { operationId: 'e'.repeat(64), deliveryId: '1', result }
  const abort = new Error('observation transaction aborted')
  db.abortNext(abort)
  const failure = await journal.observeTerminal(observation).catch(error => error)
  expect(require('../../src/web/web-indexeddb-transaction').webRecordingNoncommittedOrigin(failure)).toEqual({ original: abort })
  expect(db.rows('recordings')[0].terminalObservation).toBeUndefined()
  expect(await journal.reconcileTerminal(observation)).toBe('absent')
  db.pauseNextCompletion()
  const pending = journal.observeTerminal(observation)
  await db.requestsProcessed()
  let readFinished = false
  const read = journal.terminalRetained(observation).then(value => { readFinished = true; return value })
  await new Promise(resolve => setImmediate(resolve))
  expect(readFinished).toBe(false)
  db.complete()
  await pending
  expect(await read).toBe(true)
  await journal.observeTerminal(observation)
  expect(await journal.terminalRetained(observation)).toBe(true)
  expect(await journal.reconcileTerminal(observation)).toBe('committed')
  expect(await journal.reconcileTerminal({ ...observation, operationId: 'f'.repeat(64) })).toBe('unknown')
  expect(await journal.terminalRetained({ ...observation, operationId: 'f'.repeat(64) })).toBe(false)
  expect(await journal.terminalRetained({ ...observation, result: { ...result, hostCleanup: { state: 'uncertain' } } })).toBe(false)
  expect(await journal.reconcile(delivery)).toMatchObject({ outcome: 'committed', ordinal: 1 })
  expect(await controller.stopOrdered('sample')).toEqual(result)
  expect(db.rows('recordings')[0].terminalObservation.result).toEqual(result)
  await expect(journal.observeTerminal({ ...observation, result: { ...result, recording: { version: 1, state: 'interrupted', error: null } } })).rejects.toThrow()
  await expect(journal.observeTerminal({ ...observation, operationId: 'f'.repeat(64) })).rejects.toThrow()
})

test('terminal receipt and its storage failure commit or abort atomically', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  const storage = {
    kind: 'storage.io',
    detail: 'write failed',
    operation: 'append',
    sqliteExtendedCode: null,
    sqliteCode: null
  }
  const status = {
    recordingId: 'sample',
    phase: 'stopped',
    accepting: false,
    records: 1,
    bytes: 1,
    lostRecords: 0,
    maxBytes: declaration.maxBytes,
    maxRecords: declaration.maxRecords,
    encrypted: false,
    orderedOwner: {
      version: 1,
      producerIncarnation: 'producer',
      collectionId: '1',
      sourceGeneration: { kind: 'opaque', value: 'generation' },
      state: 'sealed',
      uncertainTail: null
    },
    runtimeFailure: null,
    collectionFailure: { ...storage, persisted: true }
  }
  const delivery = {
    id: '1',
    record: terminal,
    outcome: { version: 1, state: 'failed', error: { kind: 'storage', storage } },
    status
  }
  db.abortNext(new Error('aborted'))
  await expect(journal.commit(delivery)).rejects.toThrow('aborted')
  expect(db.rows('recordings')[0].terminal).toBe(false)
  expect(db.rows('recordings')[0].terminalOutcome).toBeUndefined()
  await journal.commit(delivery)
  expect(db.rows('recordings')[0].terminalOutcome).toMatchObject({
    record: terminal,
    outcome: delivery.outcome,
    status,
    deliveryId: '1'
  })
  await journal.retire('1')
  await journal.seal()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const controller = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 2, maxBytes: 10000 })
  await controller.acknowledge('sample', batch.token)
  expect(await controller.stopOrdered('sample')).toEqual({
    recording: delivery.outcome,
    hostCleanup: { state: 'uncertain' }
  })
  expect((await controller.status('sample')).collectionFailure).toEqual(status.collectionFailure)
})

test('legacy terminal without an observed outcome stays unavailable after acknowledgement', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: true,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  await journal.commit({ id: '1', record: terminal })
  await journal.retire('1')
  await journal.seal()
  db.mutate('recordings', row => {
    delete row.terminalOutcome
  })
  const controller = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 2, maxBytes: 10000 })
  await controller.acknowledge('sample', batch.token)
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.status('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
})

test('terminal receipt is rejected if its reservation binding no longer matches the seal', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: false,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  await journal.commit({ id: '1', record: terminal, outcome: { version: 1, state: 'finished', error: null } })
  await journal.retire('1')
  await journal.seal()
  db.mutate('recordings', row => {
    row.terminalOutcome.ownerToken = 'e'.repeat(64)
  })
  const controller = await createWebContinuationRecordingController({ database: db })
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(
    journal.reconcile({ id: '1', record: terminal, outcome: { version: 1, state: 'finished', error: null } })
  ).rejects.toThrow()
})

test('terminal commit snapshots the supplied outcome status before the transaction runs', async () => {
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: false,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  const status = {
    recordingId: 'sample',
    phase: 'stopped',
    accepting: false,
    records: 1,
    bytes: 1,
    lostRecords: 0,
    maxBytes: declaration.maxBytes,
    maxRecords: declaration.maxRecords,
    encrypted: false,
    orderedOwner: {
      version: 1,
      producerIncarnation: 'producer',
      collectionId: '1',
      sourceGeneration: { kind: 'opaque', value: 'generation' },
      state: 'sealed',
      uncertainTail: null
    },
    runtimeFailure: null,
    collectionFailure: null
  }
  const commit = journal.commit({
    id: '1',
    record: terminal,
    outcome: { version: 1, state: 'finished', error: null },
    status
  })
  status.lostRecords = 99
  await commit
  expect(db.rows('recordings')[0].terminalOutcome.status.lostRecords).toBe(0)
})

test('a terminal transaction accepts an observed live status and reports the later sealed state', async () => {
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const terminal = {
    kind: 'terminal',
    reason: { kind: 'closed' },
    tailUncertain: false,
    rejectedValues: null,
    rejectedLoss: null,
    sourceRejection: null,
    sourceFailureMember: null,
    secondaryFailure: null,
    additionalFailureCount: '0'
  }
  const status = {
    recordingId: 'sample',
    phase: 'recording',
    accepting: true,
    records: 0,
    bytes: 0,
    lostRecords: 0,
    maxBytes: declaration.maxBytes,
    maxRecords: declaration.maxRecords,
    encrypted: false,
    orderedOwner: {
      version: 1,
      producerIncarnation: 'producer',
      collectionId: '1',
      sourceGeneration: { kind: 'opaque', value: 'generation' },
      state: 'live',
      uncertainTail: null
    },
    runtimeFailure: null,
    collectionFailure: null
  }
  await journal.commit({ id: '1', record: terminal, outcome: { version: 1, state: 'finished', error: null }, status })
  await journal.retire('1')
  await journal.seal()
  const controller = await createWebContinuationRecordingController({ database: db })
  expect(await controller.status('sample')).toMatchObject({
    phase: 'stopped',
    accepting: false,
    records: 1,
    orderedOwner: { state: 'sealed' }
  })
})

test('terminal observation validation failure proves the transaction never started', async () => {
  const { webRecordingNoncommittedOrigin } = require('../../src/web/web-indexeddb-transaction')
  const db = transactionalDatabase()
  const journal = await prepared(db)
  const before = db.modes.length
  const error = await Promise.resolve().then(() => journal.observeTerminal({
    operationId: 'invalid', deliveryId: '1',
    result: { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } }
  })).catch(error => error)
  expect(webRecordingNoncommittedOrigin(error)).toBeDefined()
  expect(db.modes).toHaveLength(before)
})
