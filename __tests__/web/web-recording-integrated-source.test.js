const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { recordingDatabase, recordingBridge } = require('./web-continuation-recording-controller-helper')

const options = { signal: null, deadline: null, delivery: { itemCapacity: 8, byteCapacity: 1024, reservedControlCapacity: 1, overflowPolicy: 'error' } }

test('managed GATT callbacks cross the retained source and real journal into replay without disabling another subscriber', async () => {
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
  const existing = await database.subscribe(path, options)
  const store = recordingDatabase()
  const declaration = { id: 'integration', maxBytes: 1048576, maxRecords: 10, producerIncarnation: 'owner', collectionId: '1', sourceGeneration: 'generation', ownerEpoch: '1',
    context: { kind: 'ordered-recording', version: 1, peerId: 'origin-peer', connectionGeneration: String(path.connectionGeneration), databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence), characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence) }, delivery: null, preference: null }] } }
  const bridge = recordingBridge(store, declaration)
  const owner = backend.createRecordingSourceOwner(database, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], options, 1)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  expect((await supervisor.start()).state).toBe('active')
  boundary.emitNotification({ serviceUuid: boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.from([4, 5]) })
  supervisor.stop()
  await supervisor.retry()
  expect(supervisor.status()).toMatchObject({ state: 'finished', terminalCommitted: true })
  const controller = await createWebContinuationRecordingController({ database: store, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('integration', { maxItems: 8, maxBytes: 10000 })
  expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'terminal'])
  expect(batch.records[0].record).toMatchObject({ member: '0', membership: { member: '0', epoch: '1' }, bytes: Uint8Array.from([4, 5]), provenance: 'read-or-notification', delivery: 'unknown' })
  expect(boundary.notificationStops).toBe(0)
  await existing.remove()
  expect(boundary.notificationStops).toBe(1)
  await connection.release(); await backend.destroy()
})
