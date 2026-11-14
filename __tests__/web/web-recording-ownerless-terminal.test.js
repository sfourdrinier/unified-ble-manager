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

test('ownerless stop does not invent success after a source failure terminal is acknowledged', async () => {
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
  const declaration = { id: 'failed-source', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'generation',
    context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: String(path.connectionGeneration), databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence), characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence) }, delivery: null, preference: null }] } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: locks(), digest: async () => 'ef'.repeat(32) }, declaration, webcrypto)
  const owner = backend.createRecordingSourceOwner(database, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], options, 1)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  expect((await supervisor.start()).state).toBe('active')
  supervisor.stop('source-failed'); await supervisor.retry()
  const reader = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await reader.prepareOrdered('failed-source', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records.at(-1).record.reason.kind).toBe('source-failed')
  await reader.acknowledge('failed-source', batch.token)
  await expect(reader.stopOrdered('failed-source')).rejects.toMatchObject({ code: 'capability.unavailable' })
  await connection.release(); await backend.destroy()
})

test('real supervisor retains an explicitly supplied later outcome through the authority bridge', async () => {
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
  const declaration = { id: 'observed-stop', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'generation',
    context: { kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: String(path.connectionGeneration), databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence), characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence) }, delivery: null, preference: null }] } }
  const bridge = createWebAuthorityRecordingJournalBridge({ database: db, locks: locks(), digest: async () => 'ef'.repeat(32) }, declaration, webcrypto)
  const owner = backend.createRecordingSourceOwner(database, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], options, 1)
  // This simulated host observation is supplied explicitly, not inferred from the terminal.
  const result = { recording: { version: 1, state: 'interrupted', error: null }, hostCleanup: { state: 'released', failures: [] } }
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge, async () => ({ result }))
  expect((await supervisor.start()).state).toBe('active')
  supervisor.stop(); await supervisor.retry()
  expect(supervisor.status().state).toBe('finished')
  const reader = await createWebContinuationRecordingController({ database: db, randomToken: () => 'token' })
  const batch = await reader.prepareOrdered('observed-stop', { maxItems: 8, maxBytes: 10000 })
  await reader.acknowledge('observed-stop', batch.token)
  expect(db.rows('records')).toHaveLength(0)
  expect(await reader.stopOrdered('observed-stop')).toEqual(result)
  expect(db.rows('recordings')[0].terminalObservation.result).toEqual(result)
  expect(await bridge.terminalRetained(result)).toBe(true)
  expect(await bridge.terminalRetained({ ...result, hostCleanup: { state: 'pending' } })).toBe(false)
  await connection.release(); await backend.destroy()
})
