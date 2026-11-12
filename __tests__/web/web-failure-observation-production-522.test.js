const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
const { createWebRecordingJournalBridge, webRecordingRecord } = require('../../src/web/web-recording-journal-bridge')
const { WebRetainedRecordingPump } = require('../../src/core/web-retained-recording-pump')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { recordingDatabase, reserveJournal } = require('./web-continuation-recording-controller-helper')

const declaration = {
  id: 'observations', maxBytes: 1048576, maxRecords: 10, producerIncarnation: 'producer', collectionId: '1', sourceGeneration: 'generation', ownerEpoch: '1',
  context: { kind: 'ordered-recording', version: 2, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 1, maxBytes: 10, maxControls: 1,
    members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }] }
}

test('v2 real retained source commits exact callback rejection before terminal and replays it', async () => {
  const db = recordingDatabase()
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'heart', characteristic, lease: { enable: async () => {}, release: async () => {} } }], 1, 10, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  let journal
  const bridge = createWebRecordingJournalBridge(db, async memberEpochs => {
    journal = await reserveJournal(db, { ...declaration, memberEpochs })
    return declaration
  }, async () => ({ current: () => true, release: async () => {} }), () => journal)
  await bridge.prepare(); await bridge.open(source)
  ingress.admit(characteristic, Uint8Array.from([1]))
  ingress.admit(characteristic, Uint8Array.from([2, 3]))
  source.seal()
  const pump = new WebRetainedRecordingPump(source, bridge)
  while (await pump.runOnce() !== 'empty') {}
  await bridge.seal()
  const reader = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await reader.prepareOrdered('observations', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'failure-observation', 'terminal'])
  expect(batch.records[1].record).toEqual({ kind: 'failure-observation', failure: null, sourceRejection: { kind: 'value', bytes: '2' }, sourceFailureMember: { member: '0', epoch: '1' }, rejectedLoss: null, tailUncertain: true })
  expect(batch.records.map(row => row.metadata.deliveryId)).toEqual(['1', '2', '3'])
})

test('legacy conversion rejects a v2 observation rather than acknowledging an unrepresentable record', () => {
  expect(() => webRecordingRecord({ kind: 'failure-observation', failure: null, sourceRejection: { kind: 'value', bytes: '1' }, sourceFailureMember: null, rejectedLoss: null, tailUncertain: true }, 1)).toThrow()
})

test('byte saturation retains an immutable exact rejection until its head is acknowledged', () => {
  const ingress = new WebNotificationIngress()
  const characteristic = {}
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'heart', characteristic, lease: { enable: async () => {}, release: async () => {} } }], 2, 1, () => true, 2)
  owner.reserve()
  const { source } = owner.install(() => {})
  ingress.admit(characteristic, Uint8Array.from([1, 2]))
  const first = source.peek()
  expect(first.record).toEqual({ kind: 'failure-observation', failure: null, sourceRejection: { kind: 'value', bytes: '2' }, sourceFailureMember: { member: '0', epoch: '1' }, rejectedLoss: null, tailUncertain: true })
  first.record.sourceRejection.bytes = '99'
  first.record.sourceFailureMember.epoch = '99'
  expect(source.peek()).toEqual({ id: first.id, record: { kind: 'failure-observation', failure: null, sourceRejection: { kind: 'value', bytes: '2' }, sourceFailureMember: { member: '0', epoch: '1' }, rejectedLoss: null, tailUncertain: true } })
  expect(source.ackExact('2')).toBe(false)
  expect(source.ackExact(first.id)).toBe(true)
  source.seal()
  expect(source.peek().record.kind).toBe('terminal')
})

test('v2 journal commits an explicit omitted-observation control before source acknowledgment', async () => {
  const db = recordingDatabase()
  const record = { kind: 'failure-observations-omitted', count: '3' }
  let journal
  const bridge = createWebRecordingJournalBridge(db, async memberEpochs => {
    journal = await reserveJournal(db, { ...declaration, memberEpochs })
    return declaration
  }, async () => ({ current: () => true, release: async () => {} }), () => journal)
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  let pending = true
  const source = { peek: () => pending ? { id: '1', record } : null, ackExact: jest.fn(id => {
    expect(db.rows('records')).toHaveLength(1)
    if (id !== '1') return false
    pending = false
    return true
  }) }
  const pump = new WebRetainedRecordingPump(source, bridge)
  expect(await pump.runOnce()).toBe('committed')
  expect(source.ackExact).toHaveBeenCalledWith('1')
  expect(db.rows('records')[0].record).toEqual(record)
})

test('v1 journal refuses an omitted-observation control without acknowledging it', async () => {
  const db = recordingDatabase()
  const legacy = { ...declaration, context: { ...declaration.context, version: 1 } }
  let journal
  const bridge = createWebRecordingJournalBridge(db, async memberEpochs => {
    journal = await reserveJournal(db, { ...legacy, memberEpochs })
    return legacy
  }, async () => ({ current: () => true, release: async () => {} }), () => journal)
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  const record = { kind: 'failure-observations-omitted', count: '1' }
  const source = { peek: () => ({ id: '1', record }), ackExact: jest.fn() }
  await expect(new WebRetainedRecordingPump(source, bridge).runOnce()).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(source.ackExact).not.toHaveBeenCalled()
  expect(db.rows('records')).toHaveLength(0)
})
