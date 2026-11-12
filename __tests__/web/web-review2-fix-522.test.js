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
const { parseOrderedRecordingStartResult } = require('../../src/core/ordered-recording-start')

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
  const usePublicPartition = overrides.usePublicPartition === true
  delete overrides.usePublicPartition
  if (overrides.writable) {
    boundary.characteristic.properties = { ...boundary.characteristic.properties, write: true }
    delete overrides.writable
  }
  const storage = transactionalDatabase()
  const partition = usePublicPartition
    ? require('../../src/web').createWebRecordingPartition({
        database: storage, locks: locks(), crypto: webcrypto, digest: async () => 'ab'.repeat(32)
      })
    : null
  const partitionDependencies = partition === null ? null : require('../../src/web/web-recording-partition').webRecordingDependencies(partition)
  const liveOwners = partitionDependencies?.liveOwners ?? createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: partitionDependencies?.locks ?? locks(),
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
  const controller = partition ? await partition.createController() : await createWebContinuationRecordingController({
    database: storage,
    liveOwners,
    randomToken: () => `token-${++token}`
  })
  return { boundary, storage, liveOwners, backend, manager, connection, database, characteristic, controller }
}
const recording = id => ({ id, maxBytes: 1048576, maxRecords: 20 })

const bridgeModule = require('../../src/web/web-authority-recording-journal-bridge')

afterEach(() => jest.restoreAllMocks())

async function startRecording(f, id, maxRecords = 20) {
  const start = f.database.reserveOrderedRecordingStart()
  await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
    recording: { id, maxBytes: 1048576, maxRecords }
  })
  return start
}

async function waitFor(predicate) {
  for (let i = 0; i < 200; i++) {
    if (await predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error('condition did not settle')
}

// Drop only the completion callback: the transaction has already committed.
function withholdCompletion(storage, matches, limit = 1) {
  const original = storage.transaction.bind(storage)
  const held = []
  storage.transaction = (...args) => {
    const tx = original(...args)
    const objectStore = tx.objectStore.bind(tx)
    let selected = false
    tx.objectStore = name => {
      const store = objectStore(name)
      for (const method of ['put', 'delete', 'get']) {
        const action = store[method]
        store[method] = value => {
          if (matches({ names: args[0], mode: args[1], name, method, value })) selected = true
          return action(value)
        }
      }
      return store
    }
    let complete
    Object.defineProperty(tx, 'oncomplete', {
      set: fn => {
        complete = fn
      },
      get: () => () => {
        if (selected && held.length < limit) held.push(() => complete?.())
        else complete?.()
      }
    })
    return tx
  }
  return held
}

function identity(row) {
  return { recordingId: row.id, producerIncarnation: row.producerIncarnation, ownerEpoch: row.ownerEpoch }
}
async function advanceUntil(predicate, steps = 100) {
  for (let i = 0; i < steps && !(await predicate()); i++) await jest.advanceTimersByTimeAsync(1000)
  expect(await predicate()).toBe(true)
}
async function retainedMarker(f, id) {
  const start = await startRecording(f, id)
  const marker = start.reserveMarkerWrite()
  await marker.start(f.characteristic, Uint8Array.of(1), 'with-response')
  await waitFor(async () => (await marker.status()).write?.state === 'settled')
  const row = f.storage.rows('recordings')[0]
  await f.controller.stopOrdered(id)
  const batch = await f.controller.prepareOrdered(id, { maxItems: 8, maxBytes: 10000 })
  await f.controller.acknowledge(id, batch.token)
  return { marker, identity: identity(row) }
}

test.each([
  ['quota', () => new DOMException('private quota', 'QuotaExceededError'), 'waiting-capacity'],
  ['transient', () => new Error('private readiness'), 'failed'],
  ['forged', () => require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'forged-secret'), 'failed'],
  ['incompatible', () => require('../../src/web/web-recording-errors').webRecordingContractError('protocol.incompatible', 'restoration', 'schema'), 'failed']
])('public live recording preserves %s readiness after absent reconciliation', async (kind, failure, state) => {
  let fail = false
  let failures = 0
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) { fail = false; failures++; throw failure() }
  } })
  try {
    await startRecording(f, `reconciled-${kind}`)
    await waitFor(() => f.storage.rows('pendingIdentities').some(row => row.retired))
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    fail = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    for (let i = 0; i < 500 && !(await f.liveOwners.use(owner, async live => live.supervisor.status())).runtimeFailure; i++)
      await Promise.resolve()
    const status = await f.liveOwners.use(owner, async live => live.supervisor.status())
    expect(failures).toBe(1)
    expect(status.state).toBe(state)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
    if (kind === 'transient' || kind === 'forged') {
      await advanceUntil(() => f.storage.rows('records').some(row => row.record.bytes?.[0] === 42), 10)
      expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
    } else {
      await jest.advanceTimersByTimeAsync(10000)
      expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
    }
    const stopped = await f.controller.stopOrdered(`reconciled-${kind}`)
    expect(stopped.hostCleanup.state).toBe('released')
    const batch = await f.controller.prepareOrdered(`reconciled-${kind}`, { maxItems: 8, maxBytes: 10000 })
    expect(batch.records.filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test.each(['pending', 'already failed'])('public recording preserves %s stop cleanup failure and rejects forged host provenance', async timing => {
  const f = await publicFixture()
  let rejectStop
  let enteredStop
  const entered = new Promise(resolve => { enteredStop = resolve })
  f.boundary.characteristic.stopNotifications = () => new Promise((_resolve, reject) => {
    rejectStop = reject
    enteredStop()
  })
  try {
    const subscription = await f.characteristic.subscribe()
    const removing = subscription.remove()
    await entered
    if (timing === 'already failed') {
      rejectStop(require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'forged-secret'))
      await removing
      expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'BackendContractError' }] })
    }
    const start = f.database.reserveOrderedRecordingStart()
    const starting = start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('pending-stop')
    })
    if (timing === 'pending') {
      await waitFor(() => f.storage.rows('recordings').some(row => row.id === 'pending-stop'))
      for (let i = 0; i < 20; i++) await Promise.resolve()
      rejectStop(require('../../src/backend-contract/errors').contractError('stream.quota', 'restoration', 'forged-secret'))
      await removing
      expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'BackendContractError' }] })
    }
    const result = await starting
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed', setupFailure: {
      code: 'gatt.subscribe-failed', domain: 'gatt', operation: 'web-gatt.subscribe-pending-cleanup', platformDomain: null
    } } })
    expect(JSON.stringify(result)).not.toContain('forged-secret')
  } finally {
    f.boundary.characteristic.stopNotifications = async () => {}
    rejectStop?.(new Error('cleanup'))
    await f.manager.destroy()
  }
})

test.each(['reconcile', 'late'])('withheld terminal observation completion resumes cleanup by %s', async path => {
  const f = await publicFixture()
  let held = []
  try {
    await startRecording(f, `observation-${path}`)
    const owner = identity(f.storage.rows('recordings')[0])
    held = withholdCompletion(
      f.storage,
      x =>
        x.name === 'recordings' &&
        ((x.mode === 'readwrite' && x.method === 'put' && x.value.terminalObservation !== undefined) ||
          (path === 'late' &&
            x.mode === 'readonly' &&
            x.method === 'get' &&
            f.storage.rows('recordings')[0]?.terminalObservation !== undefined)),
      path === 'late' ? 2 : 1
    )
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.gatt.disconnect()
    await advanceUntil(() => held.length > 0, 2)
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeDefined()
    await jest.advanceTimersByTimeAsync(30001)
    if (path === 'late') {
      expect(held).toHaveLength(2)
      expect(f.liveOwners.has(owner)).toBe(true)
      await jest.advanceTimersByTimeAsync(30000)
      expect(held).toHaveLength(2)
      expect(f.liveOwners.has(owner)).toBe(true)
      held[0]()
    }
    await advanceUntil(() => !f.liveOwners.has(owner), 70)
    expect(
      f.storage.rows('readerReceipts').find(row => row.recordingId === `:ubm-owner-fence-v1:observation-${path}`).state
    ).toBe('released')
  } finally {
    held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test.each(['readiness', 'abort'])('final marker forget retries a transient %s failure', async kind => {
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const f = await publicFixture({ writable: true })
  try {
    const { marker, identity: owner } = await retainedMarker(f, 'forget-retry')
    let fail = true
    if (kind === 'readiness') {
      f.storage.ready = async () => {
        if (fail) {
          fail = false
          throw new WebRecordingReadinessError(new Error('temporary'))
        }
      }
    } else {
      f.storage.abortNext(new Error('temporary abort'))
    }
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    await expect(marker.forget()).rejects.toBeDefined()
    const failed = await f.liveOwners.use(owner, async live => live.supervisor.status())
    expect(failed.runtimeFailure.error).toBeInstanceOf(kind === 'readiness' ? WebRecordingReadinessError : Error)
    await advanceUntil(() => !f.liveOwners.has(owner), 20)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('committed provisional deletion reconciles its withheld completion', async () => {
  const peers = require('../../src/web/web-recording-peer-identity')
  const actual = peers.createWebRecordingPeerIdentityStore
  jest.spyOn(peers, 'createWebRecordingPeerIdentityStore').mockImplementation(db => ({
    ...actual(db),
    bindOwned: async () => {
      throw new Error('bind failed')
    }
  }))
  const f = await publicFixture()
  let held = []
  try {
    let owner
    held = withholdCompletion(f.storage, x => {
      const match = x.name === 'recordings' && x.method === 'delete' && x.value === 'provisional-lost'
      if (match) owner = identity(f.storage.rows('recordings')[0])
      return match
    })
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const start = f.database.reserveOrderedRecordingStart()
    const starting = start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('provisional-lost')
    })
    await advanceUntil(() => held.length > 0, 2)
    expect(f.storage.rows('recordings')).toHaveLength(0)
    await advanceUntil(() => !f.liveOwners.has(owner), 80)
    expect((await starting).kind).toBe('prepare-failed')
    jest.restoreAllMocks()
    const next = f.database.reserveOrderedRecordingStart()
    expect(
      (
        await next.start([{ memberId: 'heart', characteristic: f.characteristic }], {
          recording: recording('provisional-lost')
        })
      ).kind
    ).toBe('started')
    await f.controller.stopOrdered('provisional-lost')
  } finally {
    held.forEach(done => done())
    jest.useRealTimers()
    jest.restoreAllMocks()
    await f.manager.destroy()
  }
})

test.each(['terminal-read', 'fence-release'])('withheld %s completion does not pin retirement', async stage => {
  const f = await publicFixture({ writable: true })
  let held = []
  try {
    const { marker, identity: owner } = await retainedMarker(f, `retirement-${stage}`)
    held = withholdCompletion(f.storage, x =>
      stage === 'terminal-read'
        ? x.name === 'recordings' && x.mode === 'readonly' && x.method === 'get'
        : x.name === 'readerReceipts' &&
          x.method === 'put' &&
          x.value.state === 'released' &&
          x.value.recordingId.startsWith(':ubm-owner-fence-v1:')
    )
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    const forgotten = marker.forget().catch(() => {})
    await advanceUntil(() => held.length > 0, 2)
    await advanceUntil(() => !f.liveOwners.has(owner), 80)
    await forgotten
    expect(
      f.storage.rows('readerReceipts').find(row => row.recordingId === `:ubm-owner-fence-v1:retirement-${stage}`).state
    ).toBe('released')
  } finally {
    held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a late proven absent delivery wakes autonomous recovery after the storage deadline', async () => {
  const actual = bridgeModule.createWebAuthorityRecordingJournalBridge
  let release, entered
  const held = new Promise(resolve => { release = resolve })
  let fail = false
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) { fail = false; throw new Error('temporary readiness') }
  } })
  jest.spyOn(bridgeModule, 'createWebAuthorityRecordingJournalBridge').mockImplementation((...args) => {
    const bridge = actual(...args)
    return { ...bridge, commit: async delivery => {
      try { return await bridge.commit(delivery) } catch (error) {
        if (delivery.record.kind === 'value' && delivery.record.value[0] === 42) {
          entered = true
          await held
        }
        throw error
      }
    } }
  })
  try {
    await startRecording(f, 'late-absent')
    await waitFor(() => f.storage.rows('pendingIdentities').some(row => row.retired))
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    fail = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    await advanceUntil(() => entered, 2)
    await jest.advanceTimersByTimeAsync(31000)
    expect(f.liveOwners.has(owner)).toBe(true)
    release()
    await advanceUntil(() => !f.liveOwners.has(owner), 20)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
    expect(f.storage.rows('records').filter(row => row.record.kind === 'terminal')).toHaveLength(1)
  } finally {
    release()
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test.each([
  ['transient', () => new Error('temporary readiness'), 'failed'],
  ['quota', () => new DOMException('private quota', 'QuotaExceededError'), 'waiting-capacity']
])('a failed reconciliation preserves %s readiness until absence is established', async (kind, original, expected) => {
  let fail = false
  let f
  f = await publicFixture({ preparePartition: async () => {
    if (fail) {
      fail = false
      f.storage.abortNext(new DOMException('read failed', 'AbortError'))
      throw original()
    }
  } })
  try {
    await startRecording(f, `failed-reconcile-${kind}`)
    await waitFor(() => f.storage.rows('pendingIdentities').some(row => row.retired))
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    fail = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    if (kind === 'quota') {
      await advanceUntil(async () => (await f.liveOwners.use(owner, async live => live.supervisor.status())).state === expected, 5)
      await jest.advanceTimersByTimeAsync(5000)
      expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
      expect((await f.liveOwners.use(owner, async live => live.supervisor.status())).state).toBe('waiting-capacity')
      await f.controller.stopOrdered(`failed-reconcile-${kind}`)
    } else {
      await advanceUntil(() => !f.liveOwners.has(owner), 20)
    }
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
  } finally {
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test.each([
  ['link loss', 'connection.lost'],
  ['owner release', 'operation.disconnected'],
  ['adapter loss', 'operation.reset'],
  ['backend destroy', 'operation.cancelled-by-destroy'],
  ['manager destroy', 'operation.disconnected'],
  ['forged host error', 'gatt.subscribe-failed']
])('pending enable reports %s with its observed provenance', async (event, code) => {
  const f = await publicFixture()
  let release, reject, entered = false
  f.boundary.characteristic.startNotifications = () => new Promise((resolve, fail) => {
    entered = true
    release = resolve
    reject = fail
  })
  try {
    const start = f.database.reserveOrderedRecordingStart()
    const starting = start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
      recording: recording('pending-enable')
    })
    await waitFor(() => entered)
    if (event === 'link loss') f.boundary.gatt.disconnect()
    if (event === 'owner release') void f.connection.release()
    if (event === 'adapter loss') { f.boundary.available = false; await f.backend.adapter.currentState() }
    if (event === 'backend destroy') void f.backend.destroy()
    if (event === 'manager destroy') void f.manager.destroy()
    if (event === 'forged host error') {
      reject(require('../../src/backend-contract/errors').contractError('connection.lost', 'connection', 'forged-secret'))
    }
    const result = await starting
    if (event === 'forged host error')
      expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'BackendContractError' }] })
    expect(result).toMatchObject({ kind: 'started', receipt: { state: 'setup-failed', setupFailure: { code } } })
    if (event !== 'forged host error') expect(result.receipt.setupFailure.platformDomain).toBeNull()
    else expect(result.receipt.setupFailure.platformDomain).toBe('web-bluetooth')
    expect(JSON.stringify(result)).not.toContain('forged-secret')
  } finally {
    release?.()
    await f.manager.destroy()
  }
})

test.each(['transient', 'quota'])('a delayed reconciliation preserves %s and the exact delivery beyond the storage deadline', async kind => {
  let fail = false, intercept = false, held = []
  const f = await publicFixture({ preparePartition: async () => {
    if (fail) {
      fail = false
      throw kind === 'quota' ? new DOMException('private quota', 'QuotaExceededError') : new Error('temporary')
    }
  } })
  try {
    held = withholdCompletion(f.storage, x => intercept && x.mode === 'readonly' && x.name === 'pendingIdentities' && x.method === 'get')
    await startRecording(f, `long-reconcile-${kind}`)
    await waitFor(() => f.storage.rows('pendingIdentities').some(row => row.retired))
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    intercept = true
    fail = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    await jest.advanceTimersByTimeAsync(2000)
    expect(held).toHaveLength(1)
    await jest.advanceTimersByTimeAsync(31000)
    expect(f.liveOwners.has(owner)).toBe(true)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
    held[0]()
    if (kind === 'quota') {
      await advanceUntil(async () => (await f.liveOwners.use(owner, async live => live.supervisor.status())).state === 'waiting-capacity', 15)
      expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
      await f.controller.stopOrdered(`long-reconcile-${kind}`)
    } else {
      await advanceUntil(() => !f.liveOwners.has(owner), 20)
    }
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
    expect(f.storage.rows('records').filter(row => row.record.kind === 'terminal')).toHaveLength(1)
  } finally {
    held.forEach(done => done())
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('a readiness timeout fences a later completion before write dispatch', async () => {
  let hold = false, entered = false, release
  const gate = new Promise(resolve => { release = resolve })
  const f = await publicFixture({ preparePartition: async () => {
    if (hold) { hold = false; entered = true; await gate }
  } })
  try {
    await startRecording(f, 'held-commit-readiness')
    await waitFor(() => f.storage.rows('pendingIdentities').some(row => row.retired))
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    hold = true
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    await advanceUntil(() => entered, 2)
    await jest.advanceTimersByTimeAsync(31000)
    await advanceUntil(() => !f.liveOwners.has(owner), 20)
    release()
    await jest.advanceTimersByTimeAsync(1000)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
    expect(f.storage.rows('records').filter(row => row.record.kind === 'terminal')).toHaveLength(1)
  } finally {
    release()
    jest.useRealTimers()
    await f.manager.destroy()
  }
})

test('public recording retains a late reconciliation rejection while its delivery remains unknown', async () => {
  const journalModule = require('../../src/web/web-ordered-recording-journal')
  const actual = journalModule.createWebOrderedRecordingJournal
  const original = new AggregateError(Array.from({ length: 30 }, () => new Error('private lost commit')))
  const late = new DOMException('private read failure', 'AbortError')
  let rejectRead, writes = 0, reads = 0, resolveOutcome = false
  const held = new Promise((_resolve, reject) => { rejectRead = reject })
  jest.spyOn(journalModule, 'createWebOrderedRecordingJournal').mockImplementation((...args) => {
    const journal = actual(...args)
    return { ...journal,
      commitWithDurability: async delivery => {
        if (delivery.record.kind === 'value' && delivery.record.bytes[0] === 42 && writes++ === 0) throw original
        return journal.commitWithDurability(delivery)
      },
      reconcile: async delivery => {
        if (delivery.record.kind !== 'value' || delivery.record.bytes[0] !== 42) return journal.reconcile(delivery)
        reads++
        if (reads === 1) return held
        return { outcome: resolveOutcome ? 'absent' : 'unknown', durability: 'default' }
      }
    }
  })
  const f = await publicFixture({ usePublicPartition: true })
  let stopping
  try {
    await startRecording(f, 'late-rejected-read')
    const owner = identity(f.storage.rows('recordings')[0])
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    f.boundary.emitNotification({ serviceUuid: f.boundary.serviceUuid, serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid, characteristicOccurrence: 0, value: Uint8Array.of(42) })
    for (let i = 0; i < 5 && reads !== 1; i++) await jest.advanceTimersByTimeAsync(1000)
    expect({ phase: 'first reconciliation', reads, writes }).toMatchObject({ reads: 1, writes: 1 })
    await jest.advanceTimersByTimeAsync(6000)
    rejectRead(late)
    stopping = f.controller.stopOrdered('late-rejected-read')
    for (let i = 0; i < 15 && reads < 2; i++) await jest.advanceTimersByTimeAsync(1000)
    expect(reads).toBeGreaterThanOrEqual(2)
    const status = await f.liveOwners.use(owner, async live => live.supervisor.status())
    const bridgeFailure = status.runtimeFailure.error.errors[1]
    expect(bridgeFailure.errors[0]).toBe(original)
    expect(bridgeFailure.errors[1].message).toBe('Web recording transaction outcome remains unknown')
    expect(bridgeFailure.errors[2]).toEqual({ name: 'AbortError' })
    const publicStop = await stopping
    expect(publicStop.recording).toMatchObject({ state: 'failed', error: { kind: 'storage' } })
    expect(publicStop.recording.error.storage.detail).toContain('AbortError')
    expect(publicStop.recording.error.storage.detail).not.toContain('private')
    expect(JSON.parse(publicStop.recording.error.storage.detail)).toEqual({ errors: [
      { name: 'AggregateError', errors: [{ name: 'Error' }], errorsOmitted: true },
      { name: 'WebRecordingOutcomeTimeout' }, { name: 'AbortError' }
    ] })
    expect(writes).toBe(1)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(0)
    resolveOutcome = true
    await jest.advanceTimersByTimeAsync(1000)
    await stopping
    for (let i = 0; i < 4 && f.liveOwners.has(owner); i++) {
      await f.controller.stopOrdered('late-rejected-read')
      await jest.advanceTimersByTimeAsync(1000)
    }
    expect(f.liveOwners.has(owner)).toBe(false)
    expect(f.storage.rows('records').filter(row => row.record.bytes?.[0] === 42)).toHaveLength(1)
    expect(writes).toBe(2)
  } finally {
    rejectRead(late)
    jest.useRealTimers()
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})
