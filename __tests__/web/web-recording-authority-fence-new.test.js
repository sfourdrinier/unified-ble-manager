const { createWebRecordingJournalBridge, webRecordingOriginalFailure } = require('../../src/web/web-recording-journal-bridge')

function deferred() {
  let resolve
  const promise = new Promise(done => { resolve = done })
  return { promise, resolve }
}

test('authority loss while declaration is pending prevents dispatching the live source-open transaction', async () => {
  const declaration = deferred()
  let current = true
  const producer = {
    declare: jest.fn(() => declaration.promise),
    open: jest.fn(async () => {})
  }
  const bridge = createWebRecordingJournalBridge(
    {},
    {},
    async () => ({ current: () => current, release: async () => {} }),
    () => producer
  )
  await bridge.prepare()
  const open = bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  while (producer.declare.mock.calls.length === 0) await Promise.resolve()
  current = false
  declaration.resolve()
  const failure = await open.catch(error => error)
  expect(bridge.isDefinitiveFailure(failure)).toBe(true)
  expect(webRecordingOriginalFailure(failure)).toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  expect(producer.open).not.toHaveBeenCalled()
})

test('a completed source-open remains committed even when authority is subsequently lost', async () => {
  const opened = deferred()
  let current = true
  const producer = {
    declare: jest.fn(async () => {}),
    open: jest.fn(() => opened.promise)
  }
  const bridge = createWebRecordingJournalBridge(
    {},
    {},
    async () => ({ current: () => current, release: async () => {} }),
    () => producer
  )
  await bridge.prepare()
  const open = bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  while (producer.open.mock.calls.length === 0) await Promise.resolve()
  current = false
  opened.resolve()
  await expect(open).resolves.toBeUndefined()
  expect(bridge.canAcknowledge()).toBe(false)
})

test('real journal remains prepared when authority is lost after declaration commits', async () => {
  const { reserveJournal } = require('./web-continuation-recording-controller-helper')
  const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
  const database = transactionalDatabase()
  const declarationCommitted = deferred()
  const resume = deferred()
  let current = true
  const input = {
    id: 'authority-fence', maxBytes: 1048576, maxRecords: 10,
    producerIncarnation: 'producer', collectionId: '1', sourceGeneration: 'source', ownerEpoch: '1',
    context: {
      kind: 'ordered-recording', version: 1, peerId: 'peer', connectionGeneration: 'connection',
      databaseGeneration: 'database', maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: {
        serviceUuid: '180d', serviceOccurrence: '0', characteristicUuid: '2a37', characteristicOccurrence: '0'
      }, delivery: null, preference: null }]
    }
  }
  const journal = await reserveJournal(database, { ...input, memberEpochs: [{ member: '0', epoch: '1' }] })
  const bridge = createWebRecordingJournalBridge(
    database, input,
    async () => ({ current: () => current, release: async () => {} }),
    () => {
      const producer = journal
      return { ...producer, declare: async () => {
        await producer.declare()
        declarationCommitted.resolve()
        await resume.promise
      } }
    }
  )
  await bridge.prepare()
  const open = bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  await declarationCommitted.promise
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  current = false
  resume.resolve()
  const failure = await open.catch(error => error)
  expect(bridge.isDefinitiveFailure(failure)).toBe(true)
  expect(webRecordingOriginalFailure(failure)).toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
})

test('a real graph does not enable notifications after journal authority is lost during declaration', async () => {
  const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
  const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const { reserveJournal } = require('./web-continuation-recording-controller-helper')
  const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const provider = createWebBluetoothProvider(boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  const choosing = backend.choose({ filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] }, { signal: null, deadline: null })
  await boundary.flush(); boundary.resolveChooser()
  const peer = await choosing
  const connection = await backend.connections.connect(peer.peerId, 'fence-test', { signal: null, deadline: null })
  const graph = await backend.gatt.discover(connection.connection, { signal: null, deadline: null })
  const path = (await graph.snapshot()).characteristics[0].path
  const database = transactionalDatabase()
  const committed = deferred()
  const resume = deferred()
  let current = true
  const input = {
    id: 'graph-fence', maxBytes: 1048576, maxRecords: 10,
    producerIncarnation: 'producer', collectionId: '1', sourceGeneration: 'source', ownerEpoch: '1',
    context: {
      kind: 'ordered-recording', version: 1, peerId: String(peer.peerId), connectionGeneration: String(path.connectionGeneration),
      databaseGeneration: String(path.databaseGeneration), maxItems: 8, maxBytes: 1024, maxControls: 1,
      members: [{ memberId: '0', publicMemberId: 'heart', selector: {
        serviceUuid: String(path.serviceUuid), serviceOccurrence: String(path.serviceOccurrence),
        characteristicUuid: String(path.characteristicUuid), characteristicOccurrence: String(path.characteristicOccurrence)
      }, delivery: null, preference: null }]
    }
  }
  const journal = await reserveJournal(database, { ...input, memberEpochs: [{ member: '0', epoch: '1' }] })
  const bridge = createWebRecordingJournalBridge(database, input,
    async () => ({ current: () => current, release: async () => {} }),
    () => {
      const producer = journal
      return { ...producer, declare: async () => { await producer.declare(); committed.resolve(); await resume.promise } }
    })
  const owner = backend.createRecordingSourceOwner(graph, [{ memberId: 'heart', path, deliveryMode: 'prefer-notification' }], {
    signal: null, deadline: null, delivery: { itemCapacity: 8, byteCapacity: 1024, reservedControlCapacity: 1, overflowPolicy: 'error' }
  }, 1)
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  const starting = supervisor.start()
  await committed.promise
  current = false
  resume.resolve()
  expect(await starting).toMatchObject({ state: 'failed', sourceStarted: false })
  expect(boundary.notificationStarts).toBe(0)
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  await connection.release()
  await backend.destroy()
})
