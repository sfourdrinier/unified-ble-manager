const { createWebBluetoothProvider } = require('../../src/web/web-bluetooth-backend')
const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
const { createWebRecordingLiveOwners } = require('../../src/web/web-recording-live-owners')
const { InMemoryWebBluetoothTckBoundary } = require('../../test-support/web/in-memory-web-bluetooth-tck-boundary')
const { transactionalDatabase } = require('./web-ordered-recording-journal-helper')
const {
  attachBleBackend,
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createPublicGattDatabase } = require('../../src/public/gatt')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { webcrypto } = require('crypto')
const previousKeyRange = global.IDBKeyRange
beforeEach(() => { global.IDBKeyRange = { bound: (lower, upper, lowerOpen) => ({ lower, upper, lowerOpen }) } })
afterEach(() => { global.IDBKeyRange = previousKeyRange })
function locks() {
  const held = new Set()
  return {
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      return Promise.resolve(callback({})).finally(() => held.delete(name))
    }
  }
}

async function publicFixture(overrides = {}) {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  if (overrides.writable) {
    boundary.characteristic.properties = { ...boundary.characteristic.properties, write: true }
    delete overrides.writable
  }
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners,
    ...overrides
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  const attachedBackend = await attachBleBackend(backend, provider.descriptor.compatibility)
  const manager = await createBleManager(
    {
      attachedBackend,
      clientId: opaqueId('public-recording-client', 'client', 'web'),
      managerId: opaqueId('public-recording-manager', 'manager', 'web'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attachedBackend),
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => boundary.now() }
  )
  const choosing = backend.choose(
    { filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] },
    { signal: null, deadline: null }
  )
  await boundary.flush()
  boundary.resolveChooser()
  const peer = await choosing
  const connection = await manager.connect(peer.peerId, { signal: null, deadline: null })
  const database = await createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
  const characteristic = database.characteristic(boundary.serviceUuid, boundary.characteristicUuid)
  let token = 0
  const controller = await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return { boundary, storage, liveOwners, manager, connection, database, characteristic, controller }
}
async function startRecording(f, id, maxRecords = 20) {
  const start = f.database.reserveOrderedRecordingStart()
  await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
    recording: { id, maxBytes: 1048576, maxRecords }
  })
  return start
}

test.each([
  ['typed', () => require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'secret-operation', { domain: 'secret-domain', code: 'secret-code', safeMessage: 'secret-message', metadata: {} })],
  ['ordinary', () => new Error('secret-message')],
  ['revoked proxy', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }]
])('public start redacts host %s preparation failure', async (_name, failure) => {
  const f = await publicFixture({ preparePartition: async () => { throw failure() } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'host-failure', maxBytes: 1048576, maxRecords: 20 } })
    expect(result.kind).toBe('prepare-failed')
    expect(result.receipt.error.source.code).not.toBe('stream.quota')
    expect(JSON.stringify(result)).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

test('public start redacts an unbounded lock failure name', async () => {
  const failure = new Error('lock-secret')
  failure.name = 'name-secret'.repeat(10000)
  const f = await publicFixture({ locks: { request() { throw failure } } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'lock-failure', maxBytes: 1048576, maxRecords: 20 } })
    expect(result.kind).toBe('prepare-failed')
    expect(JSON.stringify(result)).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

test.each([
  ['typed', () => require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'secret-operation')],
  ['revoked proxy', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }],
  ['quota', () => new DOMException('secret-message', 'QuotaExceededError')]
])('public start maps a %s transaction failure', async (name, failure) => {
  const f = await publicFixture({ database: { transaction() { throw failure() } } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'transaction-failure', maxBytes: 1048576, maxRecords: 20 } })
    expect(result.kind).toBe('prepare-failed')
    expect(result.receipt.error.source.code).toBe(name === 'quota' ? 'stream.quota' : 'capability.unavailable')
    expect(JSON.stringify(result)).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

test('public start preserves a library-generated malformed digest failure', async () => {
  const f = await publicFixture({ digest: async () => 'not-a-digest' })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'bad-digest', maxBytes: 1048576, maxRecords: 20 } })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code: 'protocol.malformed', operation: 'web-recording.lock-digest' } } } })
  } finally { await f.manager.destroy() }
})

test('public start preserves a classified notification enable failure', async () => {
  const f = await publicFixture()
  const failure = new DOMException('radio-secret', 'NetworkError')
  f.boundary.characteristic.startNotifications = async () => { throw failure }
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'enable-failure', maxBytes: 1048576, maxRecords: 20 } })
    expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'NetworkError' }] })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed', setupFailure: { code: 'connection.lost' } } })
    expect(JSON.stringify(result)).not.toContain('radio-secret')
  } finally { await f.manager.destroy() }
})

test('public start does not trust a host typed notification failure', async () => {
  const f = await publicFixture()
  const failure = require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'secret-operation')
  f.boundary.characteristic.startNotifications = async () => { throw failure }
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'enable-forged', maxBytes: 1048576, maxRecords: 20 } })
    expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed', setupFailure: { code: 'gatt.subscribe-failed' } } })
    expect(JSON.stringify(result)).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

test('public start handles a revoked notification failure proxy', async () => {
  const f = await publicFixture()
  const revoked = Proxy.revocable({}, {})
  revoked.revoke()
  f.boundary.characteristic.startNotifications = async () => { throw revoked.proxy }
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'enable-proxy', maxBytes: 1048576, maxRecords: 20 } })
    expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'UnknownError' }] })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed', setupFailure: { code: 'gatt.subscribe-failed' } } })
  } finally { await f.manager.destroy() }
})

test('public start redacts an entropy provider failure before supervision begins', async () => {
  const failure = require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'entropy-secret')
  const f = await publicFixture({ crypto: { getRandomValues() { throw failure } } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const error = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'entropy-failure', maxBytes: 1048576, maxRecords: 20 } }).catch(value => value)
    expect(error).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.entropy' })
    expect(`${error.message} ${JSON.stringify(error)}`).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

test('public start retains both authority setup and release failures', async () => {
  const f = await publicFixture({
    database: { transaction() { throw new DOMException('primary-secret', 'QuotaExceededError') } },
    locks: { request(_name, _options, callback) { return Promise.resolve(callback({})).then(() => { throw new DOMException('release-secret', 'InvalidStateError') }) } }
  })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'combined-authority', maxBytes: 1048576, maxRecords: 20 } })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code: 'stream.quota', platformDomain: 'web-recording-authority', platformCode: 'AggregateError' } } } })
    const evidence = JSON.parse(result.receipt.error.source.platformMetadata.errors.value)
    expect(evidence).toEqual([{ name: 'QuotaExceededError' }, { name: 'InvalidStateError' }])
    expect(JSON.stringify(result)).not.toContain('secret')
  } finally { await f.manager.destroy() }
})

async function waitFor(predicate) {
  for (let i = 0; i < 200; i++) {
    if (await predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error('condition did not settle')
}

test('a committed provisional binding is removed only with its exact untouched declaration', async () => {
  const peers = require('../../src/web/web-recording-peer-identity')
  const actual = peers.createWebRecordingPeerIdentityStore
  let unreadable = true
  let loseCompletion
  jest.spyOn(peers, 'createWebRecordingPeerIdentityStore').mockImplementation(db => {
    const store = actual(db)
    return {
      ...store,
      bindOwned: async (...args) => {
        await store.bindOwned(...args)
        return new Promise((_resolve, reject) => { loseCompletion = reject })
      },
      readOwned: (...args) => unreadable ? Promise.reject(new Error('read unavailable')) : store.readOwned(...args)
    }
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const starting = start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: { id: 'bound-provisional', maxBytes: 1048576, maxRecords: 20 }
    })
    await waitFor(() => loseCompletion !== undefined)
    expect(f.storage.rows('readerReceipts').some(row => row.reference)).toBe(true)
    loseCompletion(new Error('completion lost'))
    expect((await starting).kind).toBe('prepare-failed')
    expect(() => start.reserveMarkerWrite()).toThrow()
    unreadable = false
    await f.controller.stopOrdered('bound-provisional')
    expect(f.storage.rows('recordings')).toHaveLength(0)
    jest.restoreAllMocks()
    const next = await startRecording(f, 'bound-provisional')
    expect(next).toBeDefined()
  } finally {
    jest.useRealTimers()
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test('timed out opens cannot accumulate underlying IndexedDB requests', async () => {
  const { defaultWebRecordingPartition } = require('../../src/web/web-recording-partition')
  const originals = Object.fromEntries(['indexedDB', 'navigator', 'crypto'].map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
  const requests = []
  Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: { open: () => {
    const request = { result: { close: jest.fn() }, error: null, transaction: null }
    requests.push(request)
    return request
  } } })
  Object.defineProperty(globalThis, 'navigator', { configurable: true, value: { locks: locks() } })
  Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
  jest.useFakeTimers()
  try {
    for (let i = 0; i < 8; i++) {
      const waiting = defaultWebRecordingPartition().catch(() => null)
      await jest.advanceTimersByTimeAsync(5001)
      await waiting
    }
    expect(requests.length).toBeLessThanOrEqual(2)
    for (const request of requests) {
      request.onsuccess()
      expect(request.result.close).toHaveBeenCalledTimes(1)
    }
    const keys = new Map([
      ['recordings', 'id'], ['records', ['recordingId', 'ordinal']],
      ['pendingIdentities', ['recordingId', 'producerIncarnation', 'collectionId', 'sourceGeneration', 'deliveryId']],
      ['readerReceipts', 'recordingId'], ['authorityReservations', 'id'],
      ['markerLedger', ['recordingId', 'producerIncarnation', 'markerId']],
      ['markerOperations', ['recordingId', 'producerIncarnation', 'operationId']],
      ['recoveryOperations', ['recordingId', 'producerIncarnation', 'operationId']],
      ['abandonments', ['recordingId', 'producerIncarnation']],
      ['terminalEvidence', 'ref'], ['terminalEvidenceState', 'key']
    ])
    const db = {
      objectStoreNames: { length: keys.size, contains: name => keys.has(name) },
      transaction: () => ({ objectStore: name => ({ keyPath: keys.get(name) }) }),
      close: jest.fn(), onversionchange: null
    }
    const next = defaultWebRecordingPartition()
    requests.at(-1).result = db
    requests.at(-1).onsuccess()
    await expect(next).resolves.toBeDefined()
    db.onversionchange()
  } finally {
    jest.useRealTimers()
    for (const [key, descriptor] of Object.entries(originals)) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor)
      else delete globalThis[key]
    }
  }
})

test.each(['observation', 'release', 'retirement'])('unsolicited cleanup retries transient %s readiness without a controller call', async stage => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { WebRecordingAuthority } = require('../../src/web/web-recording-authority')
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const make = journal.createWebOrderedRecordingJournal
  const release = WebRecordingAuthority.prototype.release
  let failed = false
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return {
      ...producer,
      observeTerminal: (...input) => {
        if (stage === 'observation' && !failed) {
          failed = true
          return Promise.reject(new WebRecordingReadinessError(new Error('temporary')))
        }
        return producer.observeTerminal(...input)
      },
      terminalRetained: (...input) => {
        if (stage === 'retirement' && !failed) {
          failed = true
          return Promise.reject(new WebRecordingReadinessError(new Error('temporary')))
        }
        return producer.terminalRetained(...input)
      }
    }
  })
  jest.spyOn(WebRecordingAuthority.prototype, 'release').mockImplementation(function () {
    if (stage === 'release' && !failed) {
      failed = true
      return Promise.reject(new WebRecordingReadinessError(new Error('temporary')))
    }
    return release.call(this)
  })
  const f = await publicFixture()
  try {
    await startRecording(f, `retry-${stage}`)
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    f.boundary.gatt.disconnect()
    await waitFor(() => failed)
    await waitFor(() => !f.liveOwners.has(identity))
    const terminal = f.storage.rows('records').find(entry => entry.record.kind === 'terminal')
    expect(terminal.record.reason.kind).toBe('connection-lost')
    expect(f.storage.rows('readerReceipts').find(entry => entry.recordingId === `:ubm-owner-fence-v1:retry-${stage}`).state).toBe('released')
  } finally {
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test.each(['held', 'absent'])('the live receipt owner reconciles an %s fence after multiple authority recoveries', async fenceState => {
  const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
  const f = await publicFixture({ writable: true })
  let resolveWrite
  f.boundary.characteristic.writeValueWithResponse = () => new Promise(resolve => { resolveWrite = resolve })
  try {
    const id = `migrated-${fenceState}`
    const start = await startRecording(f, id)
    const marker = start.reserveMarkerWrite()
    await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
    await f.controller.stopOrdered(id)
    const batch = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
    await f.controller.acknowledge(id, batch.token)
    if (fenceState === 'absent') {
      await new Promise((resolve, reject) => {
        const tx = f.storage.transaction(['readerReceipts'], 'readwrite')
        tx.objectStore('readerReceipts').delete(`:ubm-owner-fence-v1:${id}`)
        tx.oncomplete = resolve
        tx.onabort = reject
      })
    }
    const other = await createWebContinuationRecordingController({ database: f.storage, liveOwners: createWebRecordingLiveOwners() })
    await expect(other.clear(id)).rejects.toMatchObject({ operation: 'web-recording.cleanup-evidence' })
    resolveWrite()
    await waitFor(async () => (await marker.status()).write?.state === 'settled')
    await waitFor(async () => (await f.controller.stopOrdered(id)).hostCleanup.state === 'released')
    const remaining = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
    if (remaining.token !== null) await f.controller.acknowledge(id, remaining.token)
    await expect(other.clear(id)).rejects.toMatchObject({
      operation: 'web-recording.owner-fence',
      platform: { safeMessage: expect.stringContaining('marker') }
    })
    const reservations = createWebAuthorityReservations(f.storage)
    for (let i = 0; i < 2; i++) {
      const current = f.storage.rows('authorityReservations')[0]
      const owner = await reservations.recover({ id, ownerToken: String(i + 3).repeat(64), operationId: String(i + 5).repeat(64), expectedGeneration: current.generation, coordinator: { kind: 'locks', name: 'recovery' } })
      await reservations.release(owner, String(i + 7).repeat(64))
    }
    await marker.forget()
    expect(await other.clear(id)).toEqual({ cleared: true, records: 0 })
  } finally {
    resolveWrite?.()
    await f.manager.destroy()
  }
})

test.each(['aborted', 'committed'])('failed %s fence hold removes the exact bound provisional declaration', async outcome => {
  const peers = require('../../src/web/web-recording-peer-identity')
  const fence = peers.webRecordingOwnerFence
  let fail = true
  jest.spyOn(peers, 'webRecordingOwnerFence').mockImplementation(async (...args) => {
    if (args[3] === 'hold' && fail) {
      if (outcome === 'committed') await fence(...args)
      throw new Error('fence completion unavailable')
    }
    return fence(...args)
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    expect((await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: { id: 'failed-hold', maxBytes: 1048576, maxRecords: 20 }
    })).kind).toBe('prepare-failed')
    expect(() => start.reserveMarkerWrite()).toThrow()
    fail = false
    await f.controller.stopOrdered('failed-hold')
    expect(f.storage.rows('recordings')).toHaveLength(0)
    await startRecording(f, 'failed-hold')
    expect(f.storage.rows('recordings')).toHaveLength(1)
  } finally {
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})


test.each([
  ['quota', () => new DOMException('readiness-secret', 'QuotaExceededError'), 'stream.quota'],
  ['version', () => require('../../src/web/web-recording-errors').webRecordingContractError('protocol.incompatible', 'restoration', 'web-recording.schema'), 'protocol.incompatible'],
  ['hostile', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }, 'capability.unavailable']
])('public start preserves safe %s readiness classification', async (_name, failure, code) => {
  const f = await publicFixture({ preparePartition: async () => { throw failure() } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'readiness', maxBytes: 1048576, maxRecords: 20 } })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code } } } })
    expect(JSON.stringify(result)).not.toContain('readiness-secret')
  } finally { await f.manager.destroy() }
})

test.each(['forged', 'revoked', 'version-name'])('retirement retries a readiness error with %s host original', async kind => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const make = journal.createWebOrderedRecordingJournal
  let failed = false
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return { ...producer, terminalRetained: (...input) => {
      if (!failed) {
        failed = true
        let original = require('../../src/backend-contract/errors').contractError('protocol.incompatible', 'restoration', 'host-secret')
        if (kind === 'revoked') { const p = Proxy.revocable({}, {}); p.revoke(); original = p.proxy }
        if (kind === 'version-name') original = new DOMException('host-secret', 'VersionError')
        return Promise.reject(new WebRecordingReadinessError(original))
      }
      return producer.terminalRetained(...input)
    } }
  })
  const f = await publicFixture()
  try {
    await startRecording(f, `retire-${kind}`)
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    f.boundary.gatt.disconnect()
    await waitFor(() => failed)
    await waitFor(() => !f.liveOwners.has(identity))
    expect(f.storage.rows('readerReceipts').find(entry => entry.recordingId === `:ubm-owner-fence-v1:retire-${kind}`).state).toBe('released')
  } finally { jest.restoreAllMocks(); await f.manager.destroy() }
})

test('public recording reports capacity after a combined quota failure reconciles absent', async () => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { webIndexedDbQueueAbortError } = require('../../src/web/web-recording-errors')
  const make = journal.createWebOrderedRecordingJournal
  let fail = true
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return { ...producer, commitWithDurability: delivery => {
      if (fail && delivery.record.kind === 'value') return Promise.reject(webIndexedDbQueueAbortError(new DOMException('quota-secret', 'QuotaExceededError'), new Error('abort-secret')))
      return producer.commitWithDurability(delivery)
    } }
  })
  const f = await publicFixture()
  try {
    await startRecording(f, 'combined-quota')
    const row = f.storage.rows('recordings')[0]
    const identity = { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0, characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(9) })
    await waitFor(async () => f.liveOwners.use(identity, async owner => owner.supervisor.status().state === 'waiting-capacity'))
    fail = false
    await f.controller.stopOrdered('combined-quota')
  } finally { fail = false; jest.restoreAllMocks(); await f.manager.destroy() }
})

test.each([
  ['quota', () => new DOMException('readiness-secret', 'QuotaExceededError'), 'stream.quota', 'web-recording.indexeddb-transaction'],
  ['trusted schema', () => require('../../src/web/web-recording-errors').webRecordingContractError('protocol.incompatible', 'restoration', 'web-recording.schema'), 'protocol.incompatible', 'web-recording.schema'],
  ['hostile', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }, 'capability.unavailable', 'web-recording.indexeddb-transaction']
])('public start maps the underlying %s readiness wrapper', async (_name, failure, code, operation) => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const make = journal.createWebOrderedRecordingJournal
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => ({
    ...make(...args), declare: async () => { throw new WebRecordingReadinessError(failure()) }
  }))
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'wrapped-readiness', maxBytes: 1048576, maxRecords: 20 } })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code, operation } } } })
    expect(JSON.stringify(result)).not.toContain('readiness-secret')
  } finally { jest.restoreAllMocks(); await f.manager.destroy() }
})

test('public start retains library authority loss after declaration', async () => {
  const journal = require('../../src/web/web-ordered-recording-journal')
  const { WebRecordingAuthority } = require('../../src/web/web-recording-authority')
  const make = journal.createWebOrderedRecordingJournal
  const current = WebRecordingAuthority.prototype.current
  let lose = false
  jest.spyOn(WebRecordingAuthority.prototype, 'current').mockImplementation(function () {
    if (lose) { lose = false; return false }
    return current.call(this)
  })
  jest.spyOn(journal, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const producer = make(...args)
    return { ...producer, declare: async () => { await producer.declare(); lose = true } }
  })
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'lost-authority', maxBytes: 1048576, maxRecords: 20 } })
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code: 'lifecycle.invalid-state', operation: 'web-recording.authority' } } } })
  } finally { jest.restoreAllMocks(); await f.manager.destroy() }
})

test.each([
  ['message', () => new Error('stop-secret'), 'Error'],
  ['proxy', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }, 'UnknownError']
])('public stop logs bounded %s evidence and retries cleanup', async (_name, failure, errorName) => {
  const f = await publicFixture()
  let retainedOwner, terminalRetained
  try {
    await startRecording(f, 'safe-stop')
    await f.liveOwners.useByRecordingId('safe-stop', async owner => {
      retainedOwner = owner
      terminalRetained = owner.terminalRetained
      owner.supervisor.noteRetirementFailure(require('../../src/web/web-recording-errors').webRecordingContractError('stream.quota', 'restoration', 'older.failure'))
      owner.terminalRetained = async () => false
    })
    expect((await f.controller.runtimeDiagnostic('safe-stop')).lastFailure.error.operation).toBe('older.failure')
    f.boundary.nextNotificationStopFailure = failure()
    expect(await f.controller.stopOrdered('safe-stop')).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: errorName }] })
    const diagnostic = await f.controller.runtimeDiagnostic('safe-stop')
    expect(diagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'web-gatt.unsubscribe' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('stop-secret')
    expect(await f.controller.stopOrdered('safe-stop')).toMatchObject({ hostCleanup: { state: 'released' } })
    expect((await f.controller.runtimeDiagnostic('safe-stop')).lastFailure).toEqual(diagnostic.lastFailure)
  } finally {
    if (retainedOwner && terminalRetained) {
      retainedOwner.terminalRetained = terminalRetained
      const row = f.storage.rows('recordings')[0]
      await f.liveOwners.retireIfEligible({ recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }, retainedOwner)
    }
    await f.manager.destroy()
  }
})

test('public stop logs bounded nested failure evidence without host text', async () => {
  const f = await publicFixture()
  try {
    await startRecording(f, 'nested-stop')
    const child = new DOMException('child-secret', 'NetworkError')
    const root = new AggregateError([child], 'root-secret', { cause: new Error('cause-secret') })
    f.boundary.nextNotificationStopFailure = root
    expect(await f.controller.stopOrdered('nested-stop')).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', {
      errors: [{ name: 'AggregateError', errors: [{ name: 'NetworkError' }], cause: { name: 'Error' } }]
    })
    expect(await f.controller.stopOrdered('nested-stop')).toMatchObject({ hostCleanup: { state: 'released' } })
  } finally { await f.manager.destroy() }
})

test.each([
  ['quota', () => new DOMException('readiness-secret', 'QuotaExceededError'), 'stream.quota'],
  ['trusted schema', () => require('../../src/web/web-recording-errors').webRecordingContractError('protocol.incompatible', 'restoration', 'web-recording.schema'), 'protocol.incompatible'],
  ['revoked', () => { const p = Proxy.revocable({}, {}); p.revoke(); return p.proxy }, 'capability.unavailable']
])('public start classifies a %s partition failure after preparation', async (_name, failure, code) => {
  let calls = 0
  const f = await publicFixture({ preparePartition: async () => { if (++calls === 2) throw failure() } })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const result = await start.start([{ memberId: 'heart', characteristic: f.characteristic }], { recording: { id: 'real-readiness', maxBytes: 1048576, maxRecords: 20 } })
    expect(calls).toBeGreaterThanOrEqual(2)
    expect(result).toMatchObject({ kind: 'prepare-failed', receipt: { error: { kind: 'source', source: { code } } } })
    expect(JSON.stringify(result)).not.toContain('readiness-secret')
  } finally { await f.manager.destroy() }
})
