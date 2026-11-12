const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
const { createWebRecordingAuthority } = require('../../src/web/web-recording-authority')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { webcrypto } = require('crypto')

function locks() {
  const held = new Set()
  return { request(name, _options, callback) {
    if (held.has(name)) return Promise.resolve(callback(null))
    held.add(name)
    return Promise.resolve(callback({})).finally(() => held.delete(name))
  } }
}
const options = { signal: null, deadline: null, delivery: { itemCapacity: 8, byteCapacity: 1024, reservedControlCapacity: 1, overflowPolicy: 'error' } }

test('real graph owner records under an exclusive epoch and releases it only after draining', async () => {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const choosing = backend.choose({ filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] }, { signal: null, deadline: null })
  await boundary.flush(); boundary.resolveChooser()
  const selected = await choosing
  const connection = await backend.connections.connect(selected.peerId, 'recording', { signal: null, deadline: null })
  const database = await backend.gatt.discover(connection.connection, { signal: null, deadline: null })
  const path = (await database.snapshot()).characteristics[0].path
  const db = transactionalDatabase()
  const authorityOptions = { database: db, locks: locks(), digest: async () => 'ab'.repeat(32) }
  const declaration = { id: 'integration', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'generation',
    context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: String(path.connectionGeneration), databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence), characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence) }, delivery: null, preference: null }] } }
  const bridge = createWebAuthorityRecordingJournalBridge(authorityOptions, declaration, webcrypto)
  const owner = backend.createRecordingSourceOwner(database, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], options, 1)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  expect((await supervisor.start()).state).toBe('active')
  expect(db.rows('recordings')[0]).toMatchObject({ ownerEpoch: '1', ownerState: 'live', memberEpochs: [{ member: '0', epoch: '1' }] })
  expect(await createWebRecordingAuthority(authorityOptions, 'integration')).toMatchObject({ state: 'busy', lastCommitted: { state: 'live', ownerEpoch: '1' } })
  boundary.emitNotification({ serviceUuid: boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.from([4, 5]) })
  supervisor.stop(); await supervisor.retry()
  expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
  const reader = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await reader.prepareOrdered('integration', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'value', 'terminal'])
  expect(batch.records[1].record.bytes).toEqual(Uint8Array.from([4, 5]))
  const next = await createWebRecordingAuthority(authorityOptions, 'integration')
  expect(next.state).toBe('acquired')
  const original = db.rows('recordings')[0]
  expect(await next.authority.recover()).toMatchObject({ state: 'sealed', ownerEpoch: '1' })
  expect(next.authority.reservationOwner()).toMatchObject({ generation: '2', ownerEpoch: '1' })
  expect(db.rows('recordings')[0]).toEqual(original)
  await next.authority.release()
  await connection.release(); await backend.destroy()
})

test('entropy failure before lock acquisition leaves authority available', async () => {
  const db = transactionalDatabase()
  const authorityOptions = { database: db, locks: locks(), digest: async () => 'cd'.repeat(32) }
  const declaration = { id: 'entropy', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'generation',
    context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection', databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }] } }
  const bridge = createWebAuthorityRecordingJournalBridge(authorityOptions, declaration, { getRandomValues() { throw new Error('entropy unavailable') } })
  await expect(bridge.prepare()).rejects.toMatchObject({ normalized: { code: 'capability.unavailable', operation: 'web-recording.entropy' } })
  const next = await createWebRecordingAuthority(authorityOptions, 'entropy')
  expect(next.state).toBe('acquired')
  await next.authority.release()
})
