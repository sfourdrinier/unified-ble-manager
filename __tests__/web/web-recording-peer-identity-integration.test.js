const { createWebRecordingPeerIdentityStore } = require('../../src/web/web-recording-peer-identity')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
const { webcrypto } = require('crypto')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')

const peer = { version: 1, backendId: 'web-bluetooth:browser', scope: 'origin', opaqueId: 'browser-device-id' }
const declaration = {
  id: 'recording', maxBytes: 1048576, maxRecords: 10, collectionId: '1', sourceGeneration: 'source',
  context: { kind: 'ordered-recording', version: 1, peerId: 'ephemeral-peer', connectionGeneration: 'connection',
    databaseGeneration: 'graph', maxItems: 8, maxBytes: 1024, maxControls: 1,
    members: [{ memberId: '0', publicMemberId: 'heart', selector: { serviceUuid: '180d', serviceOccurrence: '0',
      characteristicUuid: '2a37', characteristicOccurrence: '0' }, delivery: null, preference: null }] }
}
const locks = { request(_name, _options, callback) { return Promise.resolve(callback({})) } }

test('persists the exact browser peer and serializes competing bindings', async () => {
  const database = transactionalDatabase()
  const first = createWebRecordingPeerIdentityStore(database)
  const second = createWebRecordingPeerIdentityStore(database)
  const [a, b] = await Promise.all([
    first.bind('recording', peer),
    second.bind('recording', { ...peer, opaqueId: 'different-device' })
  ])
  expect(a).toEqual({ state: 'stored' })
  expect(b).toEqual({ state: 'conflict' })
  expect(await second.read('recording')).toEqual(peer)
  expect(await second.bind('recording', peer)).toEqual({ state: 'already-bound' })
})

test('failed transaction does not claim that the peer is bound', async () => {
  const database = transactionalDatabase()
  const store = createWebRecordingPeerIdentityStore(database)
  database.abortNext(new Error('transaction aborted'))
  await expect(store.bind('recording', peer)).rejects.toThrow('transaction aborted')
  expect(await store.read('recording')).toBeNull()
})

test('corrupt peer rows are rejected rather than reported as absent', async () => {
  const database = transactionalDatabase()
  const store = createWebRecordingPeerIdentityStore(database)
  await store.bind('recording', peer)
  database.mutate('readerReceipts', row => { if (row.reference) row.reference.scope = 'system' })
  await expect(store.read('recording')).rejects.toThrow()
})

test('authority bridge reconciles a lost peer-binding reply before declaring the graph', async () => {
  const database = transactionalDatabase()
  const store = createWebRecordingPeerIdentityStore(database)
  const uncertainStore = {
    async bindOwned(owner, incarnation, value) { await store.bindOwned(owner, incarnation, value); throw new Error('reply lost') },
    readOwned: (owner, incarnation) => store.readOwned(owner, incarnation)
  }
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database, locks, digest: async () => 'ab'.repeat(32) }, declaration, webcrypto,
    { store: uncertainStore, reference: peer }
  )
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  expect(await store.read('recording')).toEqual(peer)
  expect(database.rows('recordings')[0]).toMatchObject({ ownerState: 'live', context: declaration.context })
  await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: true } })
  await bridge.retire('1')
  await bridge.seal()
  await bridge.releaseAuthority()
})

test('a conflicting browser peer prevents source open and revokes the prepared reservation', async () => {
  const database = transactionalDatabase()
  const store = createWebRecordingPeerIdentityStore(database)
  await store.bind('recording', { ...peer, opaqueId: 'another-device' })
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database, locks, digest: async () => 'ab'.repeat(32) }, declaration, webcrypto,
    { store, reference: peer }
  )
  await bridge.prepare()
  await expect(bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })).rejects.toThrow()
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  await bridge.releaseAuthority()
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('a malformed reconciliation read cannot authorize graph declaration', async () => {
  const database = transactionalDatabase()
  const store = {
    async bindOwned() { throw new Error('reply lost') },
    async readOwned() { return { ...peer, extra: true } }
  }
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database, locks, digest: async () => 'ab'.repeat(32) }, declaration, webcrypto,
    { store, reference: peer }
  )
  await bridge.prepare()
  await expect(bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })).rejects.toThrow()
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  await bridge.releaseAuthority()
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
})

test('peer conflict prevents source installation and releases authority', async () => {
  const database = transactionalDatabase()
  const store = createWebRecordingPeerIdentityStore(database)
  await store.bind('recording', { ...peer, opaqueId: 'another-device' })
  let lockReleased = false
  const trackedLocks = { request(_name, _options, callback) {
    return Promise.resolve(callback({})).finally(() => { lockReleased = true })
  } }
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database, locks: trackedLocks, digest: async () => 'ab'.repeat(32) }, declaration, webcrypto,
    { store, reference: peer }
  )
  const owner = { reserve: jest.fn(() => ({ memberships: () => [{ member: '0', epoch: '1' }] })), install: jest.fn(), release: jest.fn(), isCurrent: () => true }
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  await supervisor.start()
  await supervisor.retry()
  expect(owner.install).not.toHaveBeenCalled()
  expect(database.rows('recordings')[0].ownerState).toBe('prepared')
  expect(database.rows('authorityReservations')[0].phase).toBe('released')
  expect(lockReleased).toBe(true)
})

test('authority bridge snapshots the complete graph declaration before asynchronous setup', async () => {
  const database = transactionalDatabase()
  const input = JSON.parse(JSON.stringify(declaration))
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database, locks, digest: async () => 'ab'.repeat(32) }, input, webcrypto
  )
  input.context.databaseGeneration = 'different-graph'
  input.context.members[0].selector.characteristicOccurrence = '1'
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  expect(database.rows('recordings')[0].context).toEqual(declaration.context)
  await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: true } })
  await bridge.retire('1')
  await bridge.seal()
  await bridge.releaseAuthority()
})
