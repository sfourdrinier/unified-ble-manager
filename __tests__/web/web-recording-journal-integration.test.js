const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { recordingDatabase, reserveJournal } = require('./web-continuation-recording-controller-helper')

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
const delivery = n => ({
  id: String(n),
  record: {
    kind: 'value',
    ordinal: String(n),
    member: '0',
    membership: { member: '0', epoch: '1' },
    bytes: Uint8Array.from([n]),
    delivery: 'notification',
    provenance: 'read-or-notification'
  }
})
const limits = { maxItems: 1, maxBytes: 10000 }
async function setup() {
  const db = recordingDatabase()
  const journal = await reserveJournal(db, declaration)
  await journal.declare()
  await journal.open()
  let token = 0
  const controller = () =>
    createWebContinuationRecordingController({ database: db, randomToken: () => `token-${++token}` })
  return { db, journal, controller: await controller(), controllerFactory: controller }
}

test.each(['delivery identity', 'payload', 'metadata', 'ordinal', 'size', 'evidence'])(
  'retirement retains independent proof against a changed %s before prepare',
  async corruption => {
    const { db, journal, controller } = await setup()
    for (const n of [1, 3, 5]) {
      await journal.commit(delivery(n))
      await journal.retire(String(n))
    }
    db.mutate('records', row => {
      if (row.ordinal === 2) {
        if (corruption === 'delivery identity') row.metadata.deliveryId = '4'
        else if (corruption === 'payload') row.record.bytes = [4]
        else if (corruption === 'metadata') row.metadata.kind = 'tampered'
        else if (corruption === 'ordinal') row.ordinal = 3
        else if (corruption === 'size') row.byteLength++
      }
    })
    if (corruption === 'evidence') db.remove('pendingIdentities')
    if (corruption === 'evidence') {
      await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
      expect(db.rows('records')).toHaveLength(3)
      return
    }
    const first = await controller.prepareOrdered('sample', limits)
    await controller.acknowledge('sample', first.token)
    await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(db.rows('records')).toHaveLength(2)
    expect(db.rows('recordings')[0].readerCursor).toBe(1)
  }
)

test('source-first and reader-first evidence is removed only after both acknowledgements', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  await journal.retire('1')
  expect(db.rows('pendingIdentities')).toMatchObject([{ deliveryId: '1', retired: true }])
  const first = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', first.token)
  expect(db.rows('pendingIdentities')).toHaveLength(0)
  await journal.commit(delivery(2))
  const second = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', second.token)
  expect(db.rows('pendingIdentities')).toMatchObject([{ deliveryId: '2', retired: false }])
  expect(await journal.reconcile(delivery(2))).toMatchObject({ outcome: 'committed', ordinal: 2 })
  await journal.retire('2')
  expect(db.rows('pendingIdentities')).toHaveLength(0)
})

test('corruption after prepare and source retirement prevents acknowledgement and preserves both copies', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  const batch = await controller.prepareOrdered('sample', limits)
  await journal.retire('1')
  db.mutate('records', row => {
    row.record.bytes = [2]
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
  expect(db.rows('pendingIdentities')).toMatchObject([{ deliveryId: '1', retired: true }])
  expect(db.rows('recordings')[0].readerCursor).toBe(0)
})

test.each([1, 2, 99])('unsupported reader integrity version %i is rejected rather than reconstructed', async version => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  db.mutate('recordings', row => {
    row.readerIntegrity.version = version
  })
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(journal.retire('1')).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(1)
})

test('reader acknowledgements do not bypass the outstanding source evidence count quota', async () => {
  const { db, journal, controller } = await setup()
  for (let n = 1; n <= declaration.maxRecords; n++) {
    await journal.commit(delivery(n))
    const batch = await controller.prepareOrdered('sample', limits)
    await controller.acknowledge('sample', batch.token)
  }
  await expect(journal.commit(delivery(11))).rejects.toThrow('stream.quota')
  expect(db.rows('pendingIdentities')).toHaveLength(declaration.maxRecords)
  await journal.retire('1')
  expect(await journal.commit(delivery(11))).toBe(11)
})

test('outstanding source evidence also respects the byte quota after reader acknowledgement', async () => {
  const { journal, controller } = await setup()
  const large = n => ({ ...delivery(n), record: { ...delivery(n).record, bytes: new Uint8Array(300000) } })
  await journal.commit(large(1))
  const batch = await controller.prepareOrdered('sample', { maxItems: 1, maxBytes: 2_000_000 })
  await controller.acknowledge('sample', batch.token)
  await expect(journal.commit(large(2))).rejects.toThrow('stream.quota')
  await journal.retire('1')
  expect(await journal.commit(large(2))).toBe(2)
})

test.each(['pendingBytes', 'pendingCount'])(
  'isolated %s corruption cannot admit or retire evidence and capacity recovers after repair',
  async field => {
    const { db, journal, controller } = await setup()
    const large = n => ({ ...delivery(n), record: { ...delivery(n).record, bytes: new Uint8Array(300000) } })
    const count = field === 'pendingBytes' ? 1 : declaration.maxRecords
    const make = field === 'pendingBytes' ? large : delivery
    for (let n = 1; n <= count; n++) {
      await journal.commit(make(n))
      const batch = await controller.prepareOrdered('sample', { maxItems: 1, maxBytes: 2_000_000 })
      await controller.acknowledge('sample', batch.token)
    }
    const original = db.rows('recordings')[0][field]
    db.mutate('recordings', row => {
      row[field] = 1
    })
    const before = {
      recordings: db.rows('recordings'),
      records: db.rows('records'),
      pending: db.rows('pendingIdentities'),
      receipts: db.rows('readerReceipts')
    }
    await expect(journal.commit(make(count + 1))).rejects.toThrow('web-recording.reader-integrity')
    await expect(journal.retire('1')).rejects.toThrow('web-recording.reader-integrity')
    await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(db.rows('recordings')).toEqual(before.recordings)
    expect(db.rows('records')).toEqual(before.records)
    expect(db.rows('pendingIdentities')).toEqual(before.pending)
    expect(db.rows('readerReceipts')).toEqual(before.receipts)
    db.mutate('recordings', row => {
      row[field] = original
    })
    await expect(journal.commit(make(count + 1))).rejects.toThrow('stream.quota')
    await journal.retire('1')
    expect(await journal.commit(make(count + 1))).toBe(count + 1)
    expect(db.rows('pendingIdentities')).toHaveLength(count)
  }
)

test.each(['pendingCount', 'pendingBytes'])(
  'reader acknowledgement refuses corrupted independent %s evidence without deleting the prefix',
  async field => {
    const { db, journal, controller } = await setup()
    await journal.commit(delivery(1))
    const batch = await controller.prepareOrdered('sample', limits)
    const original = db.rows('recordings')[0].readerIntegrity[field]
    db.mutate('recordings', row => {
      row.readerIntegrity[field] = original + 1
    })
    const before = ['recordings', 'records', 'pendingIdentities', 'readerReceipts'].map(name => db.rows(name))
    await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
    expect(['recordings', 'records', 'pendingIdentities', 'readerReceipts'].map(name => db.rows(name))).toEqual(before)
    db.mutate('recordings', row => {
      row.readerIntegrity[field] = original
    })
    await controller.acknowledge('sample', batch.token)
    await journal.retire('1')
    expect(db.rows('pendingIdentities')).toHaveLength(0)
  }
)

test('a corrupt unread size is reported when that bounded page is reached after a partial acknowledgement', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  await journal.commit(delivery(2))
  const first = await controller.prepareOrdered('sample', limits)
  db.mutate('records', row => {
    if (row.ordinal === 2) row.byteLength++
  })
  await controller.acknowledge('sample', first.token)
  expect(db.rows('records').map(row => row.ordinal)).toEqual([2])
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('recordings')[0].readerCursor).toBe(1)
})

test('reader acknowledgement retains proof for the exact pending delivery and rejects an ordinal forged below the cursor', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  await journal.commit(delivery(2))
  const first = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', first.token)
  expect(await journal.reconcile(delivery(1))).toMatchObject({ outcome: 'committed', ordinal: 1 })
  db.mutate('pendingIdentities', row => {
    if (row.deliveryId === '2') row.ordinal = 1
  })
  db.mutate('records', row => {
    if (row.ordinal === 2) row.record.bytes = [99]
  })
  await expect(journal.reconcile(delivery(2))).rejects.toThrow()
  await expect(journal.commit(delivery(2))).rejects.toThrow()
})

test('lost producer and reader replies are reconciled across instances, with independent source retirement', async () => {
  const { db, journal, controller, controllerFactory } = await setup()
  await journal.commitWithDurability(delivery(1)) // caller loses the reply after commit
  const otherJournal = journal.anotherInstance()
  expect(await otherJournal.reconcile(delivery(1))).toMatchObject({ outcome: 'committed', ordinal: 1 })
  const first = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', first.token) // caller loses this reply
  const other = await controllerFactory()
  expect(await other.acknowledge('sample', first.token)).toEqual({ token: first.token, acknowledged: true, records: 1 })
  expect(await otherJournal.reconcile(delivery(1))).toMatchObject({ outcome: 'committed', ordinal: 1 })
  await otherJournal.retire('1')
  expect(await journal.reconcile(delivery(1))).toMatchObject({ outcome: 'unknown' })
  expect(db.rows('records')).toHaveLength(0)
})

test('retained pages survive an append and source retirement before reader acknowledgement', async () => {
  const { db, journal, controller, controllerFactory } = await setup()
  await journal.commit(delivery(1))
  await journal.retire('1')
  await journal.commit(delivery(2))
  const first = await controller.prepareOrdered('sample', limits)
  expect(first.more).toBe(true)
  await journal.commit(delivery(3))
  const other = await controllerFactory()
  expect(await other.prepareOrdered('sample', limits)).toEqual(first)
  await other.acknowledge('sample', first.token)
  const second = await controller.prepareOrdered('sample', limits)
  expect(second.records[0].ordinal).toBe(2)
  expect(second.more).toBe(true)
  await other.acknowledge('sample', second.token)
  expect(await journal.reconcile(delivery(2))).toMatchObject({ outcome: 'committed', ordinal: 2 })
  const third = await controller.prepareOrdered('sample', limits)
  expect(third.records[0].ordinal).toBe(3)
  expect(third.more).toBe(false)
  await controller.acknowledge('sample', third.token)
  expect(db.rows('recordings')[0]).toMatchObject({
    readerCursor: 3,
    recordCount: 0,
    byteCount: 0,
    readerIntegrity: { accountedBytes: 0 }
  })
})

test('an append queued while acknowledgement is unresolved preserves both accounting and the new record', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  const batch = await controller.prepareOrdered('sample', limits)
  db.pauseNext()
  const ack = controller.acknowledge('sample', batch.token)
  await db.requestsProcessed()
  const append = journal.commit(delivery(2))
  db.complete()
  await ack
  expect(await append).toBe(2)
  const stored = db.rows('records')[0]
  expect(db.rows('recordings')[0]).toMatchObject({
    readerCursor: 1,
    recordCount: 1,
    byteCount: stored.byteLength,
    readerIntegrity: { accountedBytes: stored.byteLength }
  })
  expect((await controller.prepareOrdered('sample', limits)).records[0].ordinal).toBe(2)
})

test('a corrupt pending ordinal prevents reader deletion and source reconciliation', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  const batch = await controller.prepareOrdered('sample', limits)
  db.mutate('pendingIdentities', row => {
    row.ordinal = 2
  })
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  await expect(journal.reconcile(delivery(1))).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(1)
  expect(db.rows('recordings')[0].readerCursor).toBe(0)
})

test.each(['missing', 'wrong-count'])('producer fails closed on %s reader integrity', async corruption => {
  const { db, journal } = await setup()
  await journal.commit(delivery(1))
  db.mutate('recordings', row => {
    if (corruption === 'missing') delete row.readerIntegrity
    else row.readerIntegrity.accountedBytes++
  })
  await expect(journal.commit(delivery(2))).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(1)
})

test('missing pending identity cannot masquerade as source retirement during reader acknowledgement', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  const batch = await controller.prepareOrdered('sample', limits)
  db.remove('pendingIdentities')
  await expect(controller.acknowledge('sample', batch.token)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test('a missing prepared receipt stops producer append as well as reader progress', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  await controller.prepareOrdered('sample', limits)
  db.remove('readerReceipts')
  await expect(journal.commit(delivery(2))).rejects.toThrow()
  await expect(controller.prepareOrdered('sample', limits)).rejects.toMatchObject({ code: 'protocol.malformed' })
  expect(db.rows('records')).toHaveLength(1)
})

test('a later reconciliation waits for an unresolved write before reader acknowledgement', async () => {
  const { db, journal, controller } = await setup()
  db.pauseNext()
  const write = journal.commitWithDurability(delivery(1))
  await db.requestsProcessed()
  let resolved = false
  const lookup = journal.reconcile(delivery(1)).then(result => {
    resolved = true
    return result
  })
  await new Promise(resolve => setImmediate(resolve))
  expect(resolved).toBe(false)
  db.complete()
  await write
  expect(await lookup).toMatchObject({ outcome: 'committed', ordinal: 1 })
  const batch = await controller.prepareOrdered('sample', limits)
  await controller.acknowledge('sample', batch.token)
  expect(await journal.reconcile(delivery(1))).toMatchObject({ outcome: 'committed', ordinal: 1 })
})

test('acknowledged evidence cannot be reused with a different ordinal', async () => {
  const { db, journal, controller } = await setup()
  await journal.commit(delivery(1))
  await journal.commit(delivery(2))
  for (let i = 0; i < 2; i++) {
    const batch = await controller.prepareOrdered('sample', limits)
    await controller.acknowledge('sample', batch.token)
  }
  db.mutate('pendingIdentities', row => {
    if (row.deliveryId === '2') row.ordinal = 1
  })
  await expect(journal.reconcile(delivery(2))).rejects.toThrow()
  expect(db.rows('records')).toHaveLength(0)
})

test('source retirement refuses a corrupted pending payload and preserves the record', async () => {
  const { db, journal } = await setup()
  await journal.commit(delivery(1))
  db.mutate('pendingIdentities', row => {
    row.payload = ` ${row.payload}`
  })
  await expect(journal.retire('1')).rejects.toThrow()
  expect(db.rows('pendingIdentities')).toHaveLength(1)
  expect(db.rows('records')[0].sourceRetired).toBe(false)
})
