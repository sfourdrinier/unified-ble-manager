const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { recordingDatabase, reserveJournal } = require('./web-continuation-recording-controller-helper')
const { admissionLocks } = require('./web-clear-admission-locks-helper')
const { BleError } = require('../../src/public/errors')

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
  maxRecords: 10,
  context,
  producerIncarnation: 'producer',
  collectionId: '1',
  sourceGeneration: 'generation',
  ownerEpoch: '1',
  memberEpochs: [{ member: '0', epoch: '1' }]
}
const item = n => ({
  kind: 'value',
  ordinal: String(n),
  member: '0',
  membership: { member: '0', epoch: '1' },
  bytes: Uint8Array.from([n, 0, 255]),
  delivery: 'unknown',
  provenance: 'read-or-notification'
})
const limits = { maxItems: 10, maxBytes: 10000 }

test('reads v1 and v2 rows in separate acknowledged batches', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  db.mutate('records', row => {
    if (row.ordinal === 2) row.metadata.version = 2
  })
  const first = await controller.prepareOrdered('sample', limits)
  expect(first.records.map(row => row.metadata.version)).toEqual([1])
  expect(first.more).toBe(true)
  await controller.acknowledge('sample', first.token)
  const second = await controller.prepareOrdered('sample', limits)
  expect(second.records.map(row => row.metadata.version)).toEqual([2])
  await controller.acknowledge('sample', second.token)
  expect(db.rows('records')).toHaveLength(0)
})

test('replays ordered v2 failure metadata without collapsing repeated keys', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const entries = [
    { key: 'same', value: { kind: 'text', value: 'first' } },
    { key: 'same', value: { kind: 'int', value: '9223372036854775807' } },
    { key: '__proto__', value: { kind: 'bool', value: true } }
  ]
  const record = {
    kind: 'failure-observation',
    failure: {
      code: 'platform.failure', domain: 'stream', operation: 'observe', detail: null,
      platformDomain: 'browser', platformCode: null, platformMessage: null,
      platformMetadata: {}, platformMetadataEntries: entries
    },
    sourceRejection: null, sourceFailureMember: null, rejectedLoss: null, tailUncertain: false
  }
  const { webRecordingCanonical } = require('../../src/web/web-ordered-recording-journal')
  let difference = 0
  let length = 0
  db.mutate('records', row => {
    row.metadata.version = 2
    row.record = record
    length = Buffer.byteLength(JSON.stringify({ ordinal: row.ordinal, metadata: row.metadata, record: row.record }))
    difference = length - row.byteLength
    row.byteLength = length
  })
  db.mutate('pendingIdentities', row => {
    row.payload = webRecordingCanonical(record)
    row.byteLength = length
  })
  db.mutate('recordings', row => {
    row.byteCount += difference
    row.pendingBytes += difference
    row.readerIntegrity.accountedBytes += difference
    row.readerIntegrity.pendingBytes += difference
    row.lastSourceOrdinal = null
  })
  const batch = await controller.prepareOrdered('sample', limits)
  expect(batch.records[0].record.failure.platformMetadataEntries).toEqual(entries)
  expect(await controller.prepareOrdered('sample', limits)).toEqual(batch)
  await controller.acknowledge('sample', batch.token)
})

test('rejects an unknown row protocol before preparing or acknowledging it', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.mutate('records', row => { row.metadata.version = 3 })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(0)
  expect(db.rows('records')).toHaveLength(1)
})
async function setup(peer) {
  const db = recordingDatabase()
  const locks = admissionLocks()
  const journal = await reserveJournal(db, declaration)
  let durableOwner
  if (peer !== undefined) {
    await journal.bindPeer(peer)
    const { id, generation, ownerToken, ownerEpoch } = db.rows('authorityReservations')[0]
    durableOwner = { id, generation, ownerToken, ownerEpoch }
    await require('../../src/web/web-recording-peer-identity').webRecordingOwnerFence(
      db, durableOwner, declaration.producerIncarnation, 'hold'
    )
  }
  await journal.declare()
  await journal.open()
  let token = 0
  const make = () =>
    createWebContinuationRecordingController({ database: db, locks, randomToken: () => `secret-token-${++token}` })
  return { db, locks, journal, durableOwner, controller: await make(), make }
}
test('retains an exact ordered prefix across controllers until explicit acknowledgement', async () => {
  const { db, journal, controller, make } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.retire('1')
  await journal.commit({ id: '2', record: item(2) })
  await journal.retire('2')
  const first = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
  expect(first.records[0].record.bytes).toEqual(Uint8Array.from([1, 0, 255]))
  expect(first.more).toBe(true)
  const secondController = await make()
  expect(await secondController.prepareOrdered('sample', limits)).toEqual(first)
  await expect(secondController.prepareOrdered('sample', { maxItems: 1, maxBytes: 1 })).rejects.toMatchObject({
    code: 'argument.invalid'
  })
  await expect(controller.acknowledge('sample', 'forged')).rejects.toMatchObject({ code: 'argument.invalid' })
  expect(db.rows('records')).toHaveLength(2)
  const receipt = await controller.acknowledge('sample', first.token)
  expect(receipt).toEqual({ token: first.token, acknowledged: true, records: 1 })
  expect(await secondController.acknowledge('sample', first.token)).toEqual(receipt)
  expect(db.rows('records')).toHaveLength(1)
  expect(await controller.contextOrdered('sample')).toEqual(context)
})

test('overlapping prepares serialize and a producer append cannot change the retained snapshot', async () => {
  const { db, journal, controller, make } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const other = await make()
  const [one, two] = await Promise.all([
    controller.prepareOrdered('sample', limits),
    other.prepareOrdered('sample', limits)
  ])
  expect(two).toEqual(one)
  expect(db.rows('pendingIdentities')).toHaveLength(1)
  await journal.commit({ id: '2', record: item(2) })
  expect(await other.prepareOrdered('sample', limits)).toEqual(one)
  await controller.acknowledge('sample', one.token)
  expect(db.rows('pendingIdentities')).toHaveLength(2)
  expect(db.rows('records')).toHaveLength(1)
})

test('a failed transaction never commits a receipt or advances accounting', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.abortNext(new Error('quota'))
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(0)
  const batch = await controller.prepareOrdered('sample', limits)
  db.abortNext(new Error('abort'))
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('records')).toHaveLength(1)
  expect(db.rows('recordings')[0]).toMatchObject({ readerCursor: 0, recordCount: 1 })
  expect(await controller.prepareOrdered('sample', limits)).toEqual(batch)
})

test('request success is not reported as commit', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.pauseNext()
  let settled = false
  const result = controller.prepareOrdered('sample', limits).then(value => {
    settled = true
    return value
  })
  await db.requestsProcessed()
  expect(settled).toBe(false)
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(0)
  db.complete()
  expect((await result).records).toHaveLength(1)
})

test('empty, too-small, wrong-kind, missing records, and invalid bounds do not acknowledge or create receipts', async () => {
  const { db, journal, controller } = await setup()
  expect(await controller.prepareOrdered('sample', limits)).toEqual({ token: null, records: [], bytes: 0, more: false })
  await journal.commit({ id: '1', record: item(1) })
  await expect(controller.prepareOrdered('sample', { maxItems: 1, maxBytes: 1 })).rejects.toMatchObject({
    code: 'argument.invalid'
  })
  await expect(controller.prepareOrdered('sample', { maxItems: 0, maxBytes: 1 })).rejects.toMatchObject({
    code: 'argument.invalid'
  })
  await expect(controller.prepare('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  db.mutate('records', row => {
    row.ordinal = 2
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(0)
  expect(db.rows('recordings')[0].recordCount).toBe(1)
})

test('refuses destructive or owner-controlled operations without persisted cleanup evidence', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await expect(controller.status('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.stop('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('records')).toHaveLength(1)
  expect(db.rows('pendingIdentities')).toHaveLength(1)
})

test('a new prefix has a new token and old receipts cannot acknowledge it', async () => {
  const { journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  const first = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
  await controller.acknowledge('sample', first.token)
  const second = await controller.prepareOrdered('sample', limits)
  expect(second.token).not.toBe(first.token)
  expect(await controller.acknowledge('sample', first.token)).toEqual({
    token: first.token,
    acknowledged: true,
    records: 1
  })
  expect(await controller.prepareOrdered('sample', limits)).toEqual(second)
  await controller.acknowledge('sample', second.token)
  await expect(controller.acknowledge('sample', first.token)).rejects.toMatchObject({ code: 'argument.invalid' })
})

test('corrupted owner membership, byte accounting and prepared receipts fail closed', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.mutate('records', row => {
    row.record.membership.epoch = '2'
    row.byteLength = Buffer.byteLength(
      JSON.stringify({ ordinal: row.ordinal, metadata: row.metadata, record: row.record })
    )
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  db.mutate('records', row => {
    row.record.membership.epoch = '1'
    row.byteLength = Buffer.byteLength(
      JSON.stringify({ ordinal: row.ordinal, metadata: row.metadata, record: row.record })
    )
  })
  const batch = await controller.prepareOrdered('sample', limits)
  db.mutate('readerReceipts', row => {
    if (row.recordingId === 'sample') row.pending.bytes += 1
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
  expect(db.rows('recordings')[0].readerCursor).toBe(0)
})

test('quota failures retain their typed storage causes', async () => {
  const { db, controller } = await setup()
  const quota = new Error('private details')
  quota.name = 'QuotaExceededError'
  db.abortNext(quota)
  await expect(controller.contextOrdered('sample')).rejects.toMatchObject({
    code: 'stream.quota',
    platform: { domain: 'indexeddb', code: 'QuotaExceededError' }
  })
})

test('same-size mutation of a prepared row cannot change replay or be acknowledged', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const batch = await controller.prepareOrdered('sample', limits)
  db.mutate('records', row => {
    row.record.bytes[0] = 2
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test('the final safe journal ordinal remains replayable after the producer exhausts its sequence', async () => {
  const { db, journal, controller } = await setup()
  db.mutate('recordings', row => {
    row.readerCursor = Number.MAX_SAFE_INTEGER - 1
    row.nextOrdinal = Number.MAX_SAFE_INTEGER
    row.readerIntegrity.deliveryId = '0'
    row.lastDeliveryId = '0'
  })
  await journal.commit({ id: '1', record: item(1) })
  const batch = await controller.prepareOrdered('sample', limits)
  expect(batch.records[0].ordinal).toBe(Number.MAX_SAFE_INTEGER)
  await controller.acknowledge('sample', batch.token)
  expect(await controller.prepareOrdered('sample', limits)).toEqual({ token: null, records: [], bytes: 0, more: false })
})

test('acknowledgement refuses inconsistent aggregate accounting without deleting the prefix', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const batch = await controller.prepareOrdered('sample', limits)
  db.mutate('recordings', row => {
    row.byteCount += 1
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test.each(['prepared', 'acknowledged'])('missing %s receipt fails closed', async phase => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const batch = await controller.prepareOrdered('sample', limits)
  if (phase === 'acknowledged') await controller.acknowledge('sample', batch.token)
  db.remove('readerReceipts')
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test.each([-1, 1])('partial acknowledgement detects aggregate byte corruption %i before deletion', async delta => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  const batch = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
  db.mutate('recordings', row => {
    row.byteCount += delta
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(2)
})

test.each([
  ['delivery', '2'],
  ['delivery', '1'],
  ['source', '1'],
  ['source', '0']
])('detects %s order corruption to %s across an acknowledged control', async (field, value) => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({
    id: '2',
    record: { kind: 'loss', scope: { kind: 'group' }, unit: 'values', count: '1', bytes: null }
  })
  await journal.commit({ id: '3', record: item(3) })
  for (let i = 0; i < 2; i++) {
    const batch = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
    await controller.acknowledge('sample', batch.token)
  }
  db.mutate('records', row => {
    if (field === 'delivery') row.metadata.deliveryId = value
    else row.record.ordinal = value
    row.byteLength = Buffer.byteLength(
      JSON.stringify({ ordinal: row.ordinal, metadata: row.metadata, record: row.record })
    )
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test('a post-prepare append does not hide corrupted aggregate accounting', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  const batch = await controller.prepareOrdered('sample', limits)
  expect(batch.more).toBe(false)
  await journal.commit({ id: '2', record: item(2) })
  expect(await controller.prepareOrdered('sample', limits)).toEqual(batch)
  db.mutate('recordings', row => {
    row.byteCount += 1
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(2)
})

test.each(['lastDeliveryId', 'lastSourceOrdinal'])(
  'rejects a corrupt producer high-water mark %s at the retained tail',
  async field => {
    const { db, journal, controller } = await setup()
    await journal.commit({ id: '1', record: item(1) })
    db.mutate('recordings', row => {
      row[field] = '9'
    })
    await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(db.rows('records')).toHaveLength(1)
  }
)

test('a changed more flag cannot alter a retained response, even after an append', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  const batch = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
  await journal.commit({ id: '3', record: item(3) })
  db.mutate('readerReceipts', row => {
    if (row.recordingId === 'sample') row.pending.more = false
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('does not decode an oversized malformed record beyond the byte-bounded prefix', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  const firstBytes = db.rows('records')[0].byteLength
  db.mutate('records', row => {
    if (row.ordinal === 2) {
      row.byteLength = firstBytes + 1
      row.record = 1n
    }
  })
  const batch = await controller.prepareOrdered('sample', { maxItems: 2, maxBytes: firstBytes })
  expect(batch.records).toHaveLength(1)
  expect(batch.more).toBe(true)
})

test('a retained prefix rejects an oversized malformed row without decoding it', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.commit({ id: '2', record: item(2) })
  const batch = await controller.prepareOrdered('sample', limits)
  db.mutate('records', row => {
    if (row.ordinal === 2) {
      row.byteLength = batch.bytes + 1
      row.record = 1n
    }
  })
  const stringify = jest.spyOn(JSON, 'stringify')
  try {
    await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
    await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(stringify.mock.calls.some(([value]) => value?.record === 1n)).toBe(false)
  } finally {
    stringify.mockRestore()
  }
  expect(db.rows('records')).toHaveLength(2)
})

test.each(['bigint', 'cyclic'])('malformed stored %s is a public protocol error', async kind => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.mutate('records', row => {
    if (kind === 'bigint') row.record = 1n
    else row.record.self = row.record
  })
  const error = await controller.prepareOrdered('sample', limits).catch(error => error)
  expect(error).toBeInstanceOf(BleError)
  expect(error.code).toBe('protocol.malformed')
})

test('null factory options produce a public argument error', async () => {
  const error = await createWebContinuationRecordingController(null).catch(error => error)
  expect(error).toBeInstanceOf(BleError)
  expect(error.code).toBe('argument.invalid')
})

test('legacy rows without producer integrity state fail closed instead of inventing it', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  db.mutate('recordings', row => {
    delete row.readerIntegrity
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test('shared live owner forwards concurrent stops and wakes after committed reader acknowledgement', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { db, journal } = await setup()
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  let releaseStop
  const stopped = new Promise(resolve => {
    releaseStop = resolve
  })
  const owner = {
    supervisor: { status: () => ({ authorityReleased: false }) },
    status: jest.fn(() => ({ recordingId: 'sample', phase: 'recording' })),
    stopOrdered: jest.fn(async () => {
      await stopped
      return { recording: { version: 1, state: 'waiting-drain', error: null }, hostCleanup: { state: 'pending' } }
    }),
    stopResult: () => ({
      recording: { version: 1, state: 'waiting-drain', error: null },
      hostCleanup: { state: 'pending' }
    }),
    spaceAvailable: jest.fn(async () => {})
  }
  owners.install(identity, owner)
  const make = () =>
    createWebContinuationRecordingController({ database: db, randomToken: () => 'token', liveOwners: owners })
  const [a, b] = await Promise.all([make(), make()])
  expect(await a.status('sample')).toMatchObject({ phase: 'recording' })
  const one = a.stopOrdered('sample')
  const two = b.stopOrdered('sample')
  await new Promise(resolve => setImmediate(resolve))
  expect(owner.stopOrdered).toHaveBeenCalledTimes(1)
  releaseStop()
  expect((await one).hostCleanup.state).toBe('pending')
  expect((await two).recording.state).toBe('waiting-drain')
  await journal.commit({ id: '1', record: item(1) })
  await journal.retire('1')
  const batch = await a.prepareOrdered('sample', limits)
  await b.acknowledge('sample', batch.token)
  expect(owner.spaceAvailable).toHaveBeenCalledTimes(1)
})

test('clear requires sealed and drained data, released cleanup and a released durable owner fence', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { db, locks, journal, durableOwner } = await setup({ version: 1, backendId: 'web', scope: 'origin', opaqueId: 'peer' })
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  let result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'pending' } }
  const owner = {
    supervisor: {},
    status: () => ({}),
    stopOrdered: async () => result,
    stopResult: () => result,
    terminalRetained: async () => true,
    spaceAvailable: async () => {}
  }
  owners.install({ ...identity, producerIncarnation: 'stale' }, owner)
  const controller = await createWebContinuationRecordingController({
    database: db,
    locks,
    randomToken: () => 'token',
    liveOwners: owners
  })
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  result = {
    recording: { version: 1, state: 'finished', error: null },
    hostCleanup: { state: 'released', failures: [] }
  }
  await owners.remove({ ...identity, producerIncarnation: 'stale' }, owner)
  result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'pending' } }
  owners.install(identity, owner)
  await journal.commit({
    id: '1',
    record: {
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
  })
  await journal.retire('1')
  await journal.seal()
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  result = {
    recording: { version: 1, state: 'finished', error: null },
    hostCleanup: { state: 'released', failures: [] }
  }
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('records')).toHaveLength(1)
  const batch = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', batch.token)
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await journal.observeTerminal({ operationId: 'f'.repeat(64), deliveryId: '1', result })
  await journal.releaseReservation()
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  const otherContext = await createWebContinuationRecordingController({ database: db, locks, randomToken: () => 'other' })
  await expect(otherContext.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await owners.remove(identity, owner)
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('recordings')).toHaveLength(1)
  await require('../../src/web/web-recording-peer-identity').webRecordingOwnerFence(
    db, durableOwner, identity.producerIncarnation, 'release'
  )
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
  expect(db.rows('recordings')).toHaveLength(0)
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(0)
})

test('a reader acknowledgement frees quota and wakes the retained producer only after commit', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { db, journal } = await setup()
  for (let n = 1; n <= 10; n++) {
    await journal.commit({ id: String(n), record: item(n) })
    await journal.retire(String(n))
  }
  await expect(journal.commit({ id: '11', record: item(11) })).rejects.toThrow('stream.quota')
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const pending = {
    recording: { version: 1, state: 'waiting-capacity', error: null },
    hostCleanup: { state: 'pending' }
  }
  const owner = {
    supervisor: {},
    status: () => ({}),
    stopOrdered: async () => pending,
    stopResult: () => pending,
    spaceAvailable: jest.fn(async () => {
      await journal.commit({ id: '11', record: item(11) })
    })
  }
  owners.install(identity, owner)
  const controller = await createWebContinuationRecordingController({
    database: db,
    randomToken: () => 'wake',
    liveOwners: owners
  })
  const batch = await controller.prepareOrdered('sample', { ...limits, maxItems: 1 })
  db.abortNext(new Error('transaction failed'))
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(owner.spaceAvailable).not.toHaveBeenCalled()
  await controller.acknowledge('sample', batch.token)
  expect(owner.spaceAvailable).toHaveBeenCalledTimes(1)
  expect(db.rows('records')).toHaveLength(10)
  expect(db.rows('records').some(row => row.record.ordinal === '11')).toBe(true)
})

test('a failed cleanup and an absent or stale-epoch owner never authorize clear', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { db, locks, journal } = await setup()
  await journal.commit({
    id: '1',
    record: {
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
  })
  await journal.retire('1')
  await journal.seal()
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  let failed = {
    recording: { version: 1, state: 'finished', error: null },
    hostCleanup: { state: 'released', failures: [] }
  }
  const owner = {
    supervisor: { status: () => ({ authorityReleased: false }) },
    status: () => ({}),
    stopOrdered: async () => failed,
    stopResult: () => failed,
    terminalRetained: async () => true,
    spaceAvailable: async () => {}
  }
  const controller = await createWebContinuationRecordingController({
    database: db,
    locks,
    randomToken: () => 'token',
    liveOwners: owners
  })
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  owners.install({ ...identity, ownerEpoch: '2' }, owner)
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await owners.remove({ ...identity, ownerEpoch: '2' }, owner)
  failed = {
    recording: { version: 1, state: 'finished', error: null },
    hostCleanup: {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'notification',
          error: {
            code: 'capability.unavailable',
            domain: 'restoration',
            operation: 'release',
            platform: null,
            retryability: 'caller-decides'
          }
        }
      ]
    }
  }
  owners.install(identity, owner)
  const batch = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', batch.token)
  expect((await controller.stopOrdered('sample')).hostCleanup.state).toBe('release-failed')
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('recordings')).toHaveLength(1)
  expect(db.rows('readerReceipts').filter(row => row.recordingId === 'sample')).toHaveLength(1)
})

test('a failed producer wake reports its own cause after the acknowledgement has committed', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { db, journal } = await setup()
  await journal.commit({ id: '1', record: item(1) })
  await journal.retire('1')
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const pending = {
    recording: { version: 1, state: 'waiting-capacity', error: null },
    hostCleanup: { state: 'pending' }
  }
  const owner = {
    supervisor: {},
    status: () => ({}),
    stopOrdered: async () => pending,
    stopResult: () => pending,
    spaceAvailable: jest.fn().mockRejectedValueOnce(new Error('wake failed')).mockResolvedValue(undefined)
  }
  owners.install(identity, owner)
  const controller = await createWebContinuationRecordingController({
    database: db,
    randomToken: () => 'token',
    liveOwners: owners
  })
  const batch = await controller.prepareOrdered('sample', limits)
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({
    code: 'capability.unavailable',
    platform: { domain: 'web-recording-owner', metadata: { acknowledgement: 'committed' } }
  })
  expect(db.rows('records')).toHaveLength(0)
  expect(await controller.acknowledge('sample', batch.token)).toMatchObject({ acknowledged: true, records: 1 })
  expect(owner.spaceAvailable).toHaveBeenCalledTimes(2)
  await expect(owners.remove(identity, owner)).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
})

test.each(['failed', 'interrupted'])('released %s owners can be removed without losing their outcome', async state => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const error = state === 'failed' ? { kind: 'worker-failed' } : null
  const result = { recording: { version: 1, state, error }, hostCleanup: { state: 'released', failures: [] } }
  let retained = false
  const owner = { supervisor: {}, stopResult: () => result, terminalRetained: async receipt => retained && receipt === result }
  owners.install(identity, owner)
  await expect(owners.remove(identity, owner)).rejects.toMatchObject({ normalized: { code: 'capability.unavailable' } })
  expect(owners.has(identity)).toBe(true)
  retained = true
  await owners.remove(identity, owner)
  expect(owners.has(identity)).toBe(false)
  owners.install(identity, owner)
})

test('ownerless stop does not report success when a sealed journal has a committed failure', async () => {
  const { db, journal, controller } = await setup()
  const failure = {
    code: 'source.failure', domain: 'web', operation: 'notify', detail: 'failed',
    platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {}
  }
  await journal.fail(failure)
  await journal.commit({ id: '1', record: {
    kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null,
    sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0'
  } })
  await journal.retire('1')
  await journal.seal()
  await expect(controller.stopOrdered('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('recordings')[0].failure).toEqual(failure)
})

test('clear preserves a bound recording until peer deletion is coordinated with its incarnation', async () => {
  const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
  const { createWebRecordingPeerIdentityStore } = require('../../src/web/web-recording-peer-identity')
  const { db, locks, journal } = await setup()
  const peers = createWebRecordingPeerIdentityStore(db)
  const first = { version: 1, backendId: 'web', scope: 'origin', opaqueId: 'first' }
  await peers.bind('sample', first)
  await journal.commit({ id: '1', record: {
    kind: 'terminal', reason: { kind: 'closed' }, tailUncertain: false,
    rejectedValues: null, rejectedLoss: null, sourceRejection: null,
    sourceFailureMember: null, secondaryFailure: null, additionalFailureCount: '0'
  } })
  await journal.retire('1')
  await journal.seal()
  const owners = createWebRecordingLiveOwners()
  const identity = { recordingId: 'sample', producerIncarnation: 'producer', ownerEpoch: '1' }
  const result = { recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] } }
  owners.install(identity, { supervisor: {}, stopResult: () => result, spaceAvailable: async () => {} })
  const controller = await createWebContinuationRecordingController({ database: db, locks, randomToken: () => 'token', liveOwners: owners })
  const batch = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', batch.token)
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(await peers.read('sample')).toEqual(first)
  expect(db.rows('recordings')).toHaveLength(1)
})

test('malformed stored ordered context retains its typed parser failure', async () => {
  const { db, controller } = await setup()
  db.mutate('recordings', row => { row.context.version = 99 })
  await expect(controller.contextOrdered('sample')).rejects.toMatchObject({ code: 'protocol.malformed', operation: 'continuation.recording.ordered' })
})
