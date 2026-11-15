const { admissionLocks, withAdmissionLocks } = require('./web-clear-admission-locks-helper')
const { createWebRecordingPeerIdentityStore } = require('../../src/web/web-recording-peer-identity')
const { createWebAuthorityReservations } = require('../../src/web/web-authority-reservation')
const { recordingDatabase } = require('./web-authority-reservation-helper')
beforeAll(() => { global.IDBKeyRange = { bound: (lower, upper) => ({ lower, upper }) } })
afterAll(() => { delete global.IDBKeyRange })

const peer = { version: 1, backendId: 'web', scope: 'origin', opaqueId: 'device' }
const context = {
  kind: 'ordered-recording',
  version: 1,
  peerId: 'device',
  connectionGeneration: 'c',
  databaseGeneration: 'd',
  maxItems: 8,
  maxBytes: 1024,
  maxControls: 1,
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
      delivery: null,
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
  sourceGeneration: 'source',
  memberEpochs: [{ member: '0', epoch: '1' }]
}
async function acquired(db) {
  return (
    await createWebAuthorityReservations(db).acquire({
      declaration,
      ownerToken: 'a'.repeat(64),
      operationId: 'b'.repeat(64),
      expectedGeneration: null,
      coordinator: { kind: 'locks', name: 'test' }
    })
  ).owner
}

test('binding is installed only against the exact durable producer authority', async () => {
  const db = recordingDatabase()
  const store = createWebRecordingPeerIdentityStore(db)
  const owner = await acquired(db)
  await expect(store.bindOwned(owner, 'wrong-producer', peer)).rejects.toThrow()
  expect(await store.read('sample')).toBeNull()
  expect(await store.bindOwned(owner, 'producer', peer)).toEqual({ state: 'stored' })
  expect(await store.bindOwned(owner, 'producer', peer)).toEqual({ state: 'already-bound' })
  expect(await store.bindOwned(owner, 'producer', { ...peer, opaqueId: 'other' })).toEqual({ state: 'conflict' })
  expect(db.rows('readerReceipts').find(row => row.reference)).toMatchObject({
    version: 2,
    producerIncarnation: 'producer',
    ownerEpoch: owner.ownerEpoch,
    reference: peer
  })
})

test('legacy peer rows remain readable but cannot be silently attributed to a producer', async () => {
  const db = recordingDatabase()
  const store = createWebRecordingPeerIdentityStore(db)
  await store.bind('sample', peer)
  const owner = await acquired(db)
  await expect(store.bindOwned(owner, 'producer', peer)).rejects.toThrow()
  expect(await store.read('sample')).toEqual(peer)
  expect(db.rows('readerReceipts').find(row => row.reference).version).toBe(1)
})

test('bridge binds its declared incarnation before opening and reconciles a lost binding reply', async () => {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const db = recordingDatabase()
  const actual = createWebRecordingPeerIdentityStore(db)
  let calls = 0
  const store = {
    ...actual,
    async bindOwned(...args) {
      calls++
      await actual.bindOwned(...args)
      throw new Error('lost reply')
    }
  }
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge(
    {
      database: db,
      locks: {
        request(_name, _options, fn) {
          return Promise.resolve(fn({}))
        }
      },
      digest: async () => 'ab'.repeat(32)
    },
    input,
    webcrypto,
    { store, reference: peer }
  )
  await bridge.prepare()
  expect(await actual.read('sample')).toBeNull()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  const row = db.rows('recordings')[0]
  expect(calls).toBe(1)
  expect(db.rows('readerReceipts').find(value => value.reference)).toMatchObject({
    version: 2,
    producerIncarnation: row.producerIncarnation,
    ownerEpoch: row.ownerEpoch,
    reference: peer
  })
  await finish(bridge)
})

async function finish(bridge) {
  await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
  await bridge.retire('1')
  await bridge.seal()
  await bridge.observeTerminal({
    result: {
      recording: { version: 1, state: 'finished', error: null },
      hostCleanup: { state: 'released', failures: [] }
    }
  })
  await bridge.releaseAuthority()
}
async function sealedBinding() {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const db = recordingDatabase(),
    store = createWebRecordingPeerIdentityStore(db)
  const held = new Set()
  const locks = {
    request(name, _options, fn) {
      if (held.has(name)) return Promise.resolve(fn(null))
      held.add(name)
      return Promise.resolve(fn({})).finally(() => held.delete(name))
    }
  }
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge(
    {
      database: db,
      locks,
      digest: async () => 'ab'.repeat(32)
    },
    input,
    webcrypto,
    { store, reference: peer }
  )
  await bridge.prepare()
  await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
  await finish(bridge)
  const controller = await createWebContinuationRecordingController({ database: db, locks, randomToken: () => 'token' })
  const batch = await controller.prepareOrdered('sample', { maxItems: 8, maxBytes: 10000 })
  await controller.acknowledge('sample', batch.token)
  return { db, store, controller }
}
test('clear atomically removes a released, drained recording and its matching incarnation binding', async () => {
  const { db, store, controller } = await sealedBinding()
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
  expect(db.rows('recordings')).toEqual([])
  expect(await store.read('sample')).toBeNull()
})

test.each(['missing', 'legacy', 'corrupt', 'newer'])(
  'clear refuses %s peer evidence without deleting the journal',
  async mode => {
    const { db, controller } = await sealedBinding()
    if (mode === 'missing') db.remove('readerReceipts')
    else
      db.mutate('readerReceipts', row => {
        if (!row.reference) return
        if (mode === 'legacy') {
          row.version = 1
          delete row.producerIncarnation
          delete row.ownerEpoch
        }
        if (mode === 'corrupt') row.reference.scope = 'system'
        if (mode === 'newer') row.producerIncarnation = 'newer'
      })
    await expect(controller.clear('sample')).rejects.toThrow()
    expect(db.rows('recordings')).toHaveLength(1)
    expect(db.rows('authorityReservations')).toHaveLength(1)
    if (mode !== 'missing') expect(db.rows('readerReceipts').find(row => row.reference)).toBeDefined()
  }
)

test('clear reconciles a transaction that committed but reported an abort', async () => {
  const { db, controller, store } = await sealedBinding()
  const transaction = db.transaction.bind(db)
  let lost = false
  db.transaction = (...args) => {
    const tx = transaction(...args)
    const objectStore = tx.objectStore.bind(tx)
    let clearing = false
    tx.objectStore = name => {
      const object = objectStore(name)
      if (name === 'recordings') {
        const remove = object.delete
        object.delete = key => {
          clearing = true
          return remove(key)
        }
      }
      return object
    }
    let complete
    Object.defineProperty(tx, 'oncomplete', {
      set: value => {
        complete = value
      },
      get: () => () => {
        if (clearing && !lost) {
          lost = true
          tx.onabort()
        } else complete?.()
      }
    })
    return tx
  }
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
  expect(lost).toBe(true)
  expect(db.rows('recordings')).toEqual([])
  expect(await store.read('sample')).toBeNull()
})

test('revoked provisional authority can rebind the same peer for its successor, while a stale producer is fenced', async () => {
  const db = recordingDatabase(),
    store = createWebRecordingPeerIdentityStore(db)
  const reservations = createWebAuthorityReservations(db),
    first = await acquired(db)
  await store.bindOwned(first, 'producer', peer)
  await reservations.revokeProvisional(first, 'c'.repeat(64))
  const successor = (
    await reservations.acquire({
      declaration: { ...declaration, producerIncarnation: 'successor' },
      ownerToken: 'd'.repeat(64),
      operationId: 'e'.repeat(64),
      expectedGeneration: first.generation,
      coordinator: { kind: 'locks', name: 'test' }
    })
  ).owner
  await expect(store.bindOwned(first, 'producer', peer)).rejects.toThrow()
  expect(await store.bindOwned(successor, 'successor', peer)).toEqual({ state: 'stored' })
  expect(db.rows('readerReceipts').find(row => row.reference)).toMatchObject({
    producerIncarnation: 'successor',
    ownerEpoch: successor.ownerEpoch
  })
})

test('an aborted clear preserves both journal and binding', async () => {
  const { db, controller, store } = await sealedBinding()
  const transaction = db.transaction.bind(db)
  let injected = false
  db.transaction = (...args) => {
    const tx = transaction(...args),
      objectStore = tx.objectStore.bind(tx)
    tx.objectStore = name => {
      const object = objectStore(name)
      if (name === 'recordings') {
        const remove = object.delete
        object.delete = key => {
          if (!injected) {
            injected = true
            tx.abort()
          }
          return remove(key)
        }
      }
      return object
    }
    return tx
  }
  await expect(controller.clear('sample')).rejects.toThrow()
  expect(injected).toBe(true)
  expect(db.rows('recordings')).toHaveLength(1)
  expect(await store.read('sample')).toEqual(peer)
})

test('a delayed clear keeps ownership until it finishes before an ID can be reused', async () => {
  const { db, controller, store } = await sealedBinding()
  const transaction = db.transaction.bind(db)
  let release, observed
  const paused = new Promise(resolve => {
    observed = resolve
  })
  let intercept = true
  db.transaction = (...args) => {
    const tx = transaction(...args)
    if (intercept && args[0].length === 3 && args[0].includes('authorityReservations')) {
      intercept = false
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          complete = fn
        },
        get: () => () => {
          release = complete
          observed()
        }
      })
    }
    return tx
  }
  const stale = controller.clear('sample')
  await paused
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  release()
  expect(await stale).toEqual({ cleared: true, records: 0 })
  const successor = (
    await createWebAuthorityReservations(db).acquire({
      declaration: { ...declaration, producerIncarnation: 'new-producer' },
      ownerToken: 'd'.repeat(64),
      operationId: 'e'.repeat(64),
      expectedGeneration: null,
      coordinator: { kind: 'locks', name: 'test' }
    })
  ).owner
  const nextPeer = { ...peer, opaqueId: 'new-device' }
  await store.bindOwned(successor, 'new-producer', nextPeer)
  expect(db.rows('recordings')[0].producerIncarnation).toBe('new-producer')
  expect(await store.read('sample')).toEqual(nextPeer)
})

test('competing peer bindings serialize, and an aborted binding leaves no peer', async () => {
  const db = recordingDatabase(),
    store = createWebRecordingPeerIdentityStore(db),
    owner = await acquired(db)
  db.abortNext(new Error('write aborted'))
  await expect(store.bindOwned(owner, 'producer', peer)).rejects.toThrow('write aborted')
  expect(await store.read('sample')).toBeNull()
  const [first, second] = await Promise.all([
    store.bindOwned(owner, 'producer', peer),
    store.bindOwned(owner, 'producer', { ...peer, opaqueId: 'other' })
  ])
  expect(first.state).toBe('stored')
  expect(second.state).toBe('conflict')
  expect(await store.read('sample')).toEqual(peer)
})

test('a lost clear completion is resolved from its durable receipt after timeout', async () => {
  const { db, controller } = await sealedBinding()
  const transaction = db.transaction.bind(db)
  let lost = false,
    deliverCompletion
  db.transaction = (...args) => {
    const tx = transaction(...args),
      objectStore = tx.objectStore.bind(tx)
    let clearing = false
    tx.objectStore = name => {
      const object = objectStore(name)
      if (name === 'recordings') {
        const remove = object.delete
        object.delete = key => {
          clearing = true
          return remove(key)
        }
      }
      return object
    }
    let complete
    Object.defineProperty(tx, 'oncomplete', {
      set: value => {
        complete = value
      },
      get: () => () => {
        if (clearing && !lost) {
          lost = true
          deliverCompletion = complete
        } else complete?.()
      }
    })
    return tx
  }
  jest.useFakeTimers()
  try {
    const result = controller.clear('sample')
    for (let i = 0; i < 100 && !lost; i++) await Promise.resolve()
    expect(lost).toBe(true)
    await jest.advanceTimersByTimeAsync(5000)
    expect(await result).toEqual({ cleared: true, records: 0 })
    expect(db.rows('recordings')).toEqual([])
    deliverCompletion()
    await Promise.resolve()
  } finally {
    jest.useRealTimers()
  }
})

test('public Web start binds the selected peer, and public clear permits safe ID reuse', async () => {
  const { webcrypto } = require('crypto')
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
  const boundary = new InMemoryWebBluetoothTckBoundary(),
    db = transactionalDatabase(),
    liveOwners = createWebRecordingLiveOwners()
  const held = new Set()
  const locks = {
    request(name, _options, fn) {
      if (held.has(name)) return Promise.resolve(fn(null))
      held.add(name)
      return Promise.resolve(fn({})).finally(() => held.delete(name))
    }
  }
  const provider = createWebBluetoothProvider(boundary, {
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'ab'.repeat(32),
    liveOwners
  })
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  const attachedBackend = await attachBleBackend(backend, provider.descriptor.compatibility)
  const manager = await createBleManager(
    {
      attachedBackend,
      clientId: opaqueId('peer-clear-client', 'client', 'web'),
      managerId: opaqueId('peer-clear-manager', 'manager', 'web'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attachedBackend),
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  try {
    const choosing = backend.choose(
      { filters: [], acceptAllDevices: true, optionalServices: [boundary.serviceUuid] },
      { signal: null, deadline: null }
    )
    await boundary.flush()
    boundary.resolveChooser()
    const chosen = await choosing
    const connection = await manager.connect(chosen.peerId, { signal: null, deadline: null })
    const gatt = await createPublicGattDatabase(await connection.discover({ signal: null, deadline: null }))
    const characteristic = gatt.characteristic(boundary.serviceUuid, boundary.characteristicUuid)
    const controller = await createWebContinuationRecordingController({
      database: db,
      locks,
      liveOwners,
      randomToken: () => 'token'
    })
    const startAndClear = async () => {
      const start = gatt.reserveOrderedRecordingStart()
      expect(
        (
          await start.start([{ memberId: 'heart', characteristic }], {
            recording: { id: 'public-reuse', maxBytes: 1048576, maxRecords: 20 }
          })
        ).kind
      ).toBe('started')
      const row = db.rows('recordings')[0]
      expect(db.rows('readerReceipts').find(value => value.reference)).toMatchObject({
        version: 2,
        producerIncarnation: row.producerIncarnation,
        ownerEpoch: row.ownerEpoch,
        reference: { scope: 'origin', opaqueId: boundary.device.id }
      })
      expect((await controller.stopOrdered('public-reuse')).hostCleanup.state).toBe('released')
      const batch = await controller.prepareOrdered('public-reuse', { maxItems: 8, maxBytes: 10000 })
      await controller.acknowledge('public-reuse', batch.token)
      expect(await controller.clear('public-reuse')).toEqual({ cleared: true, records: 0 })
      return row.producerIncarnation
    }
    const first = await startAndClear()
    expect(await startAndClear()).not.toBe(first)
    await connection.release()
  } finally {
    await manager.destroy()
  }
})

test('legacy conflict fences source installation and does not register a live owner', async () => {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')
  const db = recordingDatabase(),
    store = createWebRecordingPeerIdentityStore(db)
  await store.bind('sample', peer)
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const onDeclared = jest.fn()
  const bridge = createWebAuthorityRecordingJournalBridge(
    {
      database: db,
      locks: {
        request(_name, _options, fn) {
          return Promise.resolve(fn({}))
        }
      },
      digest: async () => 'ab'.repeat(32)
    },
    input,
    webcrypto,
    { store, reference: peer },
    onDeclared
  )
  const owner = {
    reserve: jest.fn(() => ({ memberships: () => [{ member: '0', epoch: '1' }] })),
    install: jest.fn(),
    release: jest.fn(async () => {}),
    isCurrent: () => true
  }
  const supervisor = new WebOrderedRecordingSupervisor(owner, bridge)
  await supervisor.start()
  await supervisor.retry()
  expect(owner.install).not.toHaveBeenCalled()
  expect(onDeclared).not.toHaveBeenCalled()
  expect(owner.release).toHaveBeenCalled()
  expect(db.rows('authorityReservations')[0].phase).toBe('released')
  expect(await store.read('sample')).toEqual(peer)
})

test('a corrupt durable acquisition receipt cannot authorize a peer binding', async () => {
  const db = recordingDatabase(),
    store = createWebRecordingPeerIdentityStore(db),
    owner = await acquired(db)
  db.mutate('authorityReservations', row => {
    row.receipt.transition = 'release'
  })
  await expect(store.bindOwned(owner, 'producer', peer)).rejects.toThrow()
  expect(await store.read('sample')).toBeNull()
})

test('an uncertain bind with an unreadable reconciliation cannot install an owner', async () => {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const db = recordingDatabase(),
    actual = createWebRecordingPeerIdentityStore(db)
  const store = {
    ...actual,
    async bindOwned() {
      throw new Error('write reply lost')
    },
    async readOwned() {
      throw new Error('read failed')
    }
  }
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const onDeclared = jest.fn()
  const bridge = createWebAuthorityRecordingJournalBridge(
    {
      database: db,
      locks: {
        request(_name, _options, fn) {
          return Promise.resolve(fn({}))
        }
      },
      digest: async () => 'ab'.repeat(32)
    },
    input,
    webcrypto,
    { store, reference: peer },
    onDeclared
  )
  await bridge.prepare()
  await expect(bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })).rejects.toThrow()
  expect(onDeclared).not.toHaveBeenCalled()
  expect(await actual.read('sample')).toBeNull()
  await bridge.releaseAuthority()
})

test('binding refuses a corrupt authority policy even if reservation fields match', async () => {
  const db = recordingDatabase()
  const store = createWebRecordingPeerIdentityStore(db)
  const owner = await acquired(db)
  db.mutate('readerReceipts', row => {
    if (row.recordingId === ':ubm-authority-policy-v1') row.protocol = 'bad'
  })
  await expect(store.bindOwned(owner, 'producer', peer)).rejects.toThrow()
  expect(await store.read('sample')).toBeNull()
})

test('two database wrappers serialize unresolved clear allocation', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false
  const locks = {
    request(_name, _options, callback) {
      if (held) return Promise.resolve(callback(null))
      held = true
      return Promise.resolve(callback({})).finally(() => {
        held = false
      })
    }
  }
  let releaseReady
  let readinessCalls = 0
  const firstDatabase = {
    transaction: db.transaction.bind(db),
    ready() {
      readinessCalls++
      return new Promise(resolve => {
        releaseReady = resolve
      })
    }
  }
  const secondDatabase = { transaction: db.transaction.bind(db) }
  const first = await createWebContinuationRecordingController({ database: firstDatabase, locks })
  const second = await createWebContinuationRecordingController({ database: secondDatabase, locks })
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const pending = first.clear('sample').catch(error => error)
    for (let i = 0; i < 30 && !releaseReady; i++) await jest.advanceTimersByTimeAsync(0)
    expect(readinessCalls).toBe(1)
    await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(db.rows('recordings')).toHaveLength(1)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await pending).toMatchObject({ code: 'capability.unavailable' })
    await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
    releaseReady()
    for (let i = 0; i < 30 && held; i++) await jest.advanceTimersByTimeAsync(0)
    expect(held).toBe(false)
    expect(await second.clear('sample')).toEqual({ cleared: true, records: 0 })
  } finally {
    releaseReady?.()
    jest.useRealTimers()
  }
})

test('a lost allocation completion retains one partition owner until its late result arrives', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false,
    lateCompletion,
    allocations = 0,
    transactions = 0
  const locks = {
    request(_name, _options, callback) {
      if (held) return Promise.resolve(callback(null))
      held = true
      return Promise.resolve(callback({})).finally(() => {
        held = false
      })
    }
  }
  const transaction = db.transaction.bind(db)
  const firstDatabase = {
    transaction(...args) {
      transactions++
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let allocation = false
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') {
              allocation = true
              allocations++
            }
            return put(value)
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
          if (allocation) lateCompletion = complete
          else complete?.()
        }
      })
      return tx
    }
  }
  const secondDatabase = {
    transaction(...args) {
      transactions++
      return transaction(...args)
    }
  }
  const first = await createWebContinuationRecordingController({ database: firstDatabase, locks })
  const second = await createWebContinuationRecordingController({ database: secondDatabase, locks })
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const result = first.clear('sample').catch(error => error)
    for (let i = 0; i < 100 && !lateCompletion; i++) await jest.advanceTimersByTimeAsync(0)
    expect(allocations).toBe(1)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await result).toMatchObject({ code: 'capability.unavailable' })
    const before = transactions
    const retries = await Promise.all(Array.from({ length: 100 }, () => second.clear('sample').catch(error => error)))
    expect(retries.every(error => error.code === 'capability.unavailable')).toBe(true)
    expect(transactions).toBe(before)
    expect(allocations).toBe(1)
    expect(db.rows('recordings')).toHaveLength(1)
    lateCompletion()
    for (let i = 0; i < 100 && held; i++) await jest.advanceTimersByTimeAsync(0)
    expect(held).toBe(false)
    expect(await second.clear('sample')).toEqual({ cleared: true, records: 0 })
  } finally {
    lateCompletion?.()
    jest.useRealTimers()
  }
})

test('a timed-out lock request cannot later acquire or start clear work', async () => {
  const { acquireWebRecordingClearLock } = require('../../src/web/web-recording-clear-lock')
  await expect(acquireWebRecordingClearLock(undefined)).rejects.toMatchObject({
    normalized: { code: 'capability.unsupported' }
  })
  let callback, finish
  const locks = {
    request(_name, _options, fn) {
      callback = fn
      return new Promise(resolve => {
        finish = resolve
      })
    }
  }
  jest.useFakeTimers()
  try {
    const result = acquireWebRecordingClearLock(locks).catch(error => error)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await result).toMatchObject({ normalized: { code: 'capability.unavailable' } })
    await expect(acquireWebRecordingClearLock(locks)).rejects.toMatchObject({
      normalized: { code: 'capability.unavailable' }
    })
    expect(callback({})).toBeUndefined()
    finish()
    await Promise.resolve()
  } finally {
    jest.useRealTimers()
  }
})

test('a timed-out deletion keeps partition ownership and its late commit retains the exact receipt', async () => {
  const { db } = await sealedBinding()
  const producerIncarnation = db.rows('recordings')[0].producerIncarnation
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false,
    holdNext = false,
    resume,
    transactions = 0
  const locks = {
    request(_name, _options, callback) {
      if (held) return Promise.resolve(callback(null))
      held = true
      return Promise.resolve(callback({})).finally(() => {
        held = false
      })
    }
  }
  const transaction = db.transaction.bind(db)
  const firstDatabase = {
    ready() {
      if (!holdNext) return Promise.resolve()
      holdNext = false
      return new Promise(resolve => {
        resume = resolve
      })
    },
    transaction(...args) {
      transactions++
      const tx = transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdNext = true
            return put(value)
          }
        }
        return store
      }
      return tx
    }
  }
  const secondDatabase = {
    transaction(...args) {
      transactions++
      return transaction(...args)
    }
  }
  const first = await createWebContinuationRecordingController({ database: firstDatabase, locks })
  const second = await createWebContinuationRecordingController({ database: secondDatabase, locks })
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const result = first.clear('sample').catch(error => error)
    for (let i = 0; i < 100 && !resume; i++) await jest.advanceTimersByTimeAsync(0)
    expect(resume).toBeDefined()
    await jest.advanceTimersByTimeAsync(5001)
    expect(await result).toMatchObject({ code: 'capability.unavailable' })
    const before = transactions
    await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(transactions).toBe(before)
    resume()
    for (let i = 0; i < 100 && held; i++) await jest.advanceTimersByTimeAsync(0)
    expect(held).toBe(false)
    expect(db.rows('recordings')).toHaveLength(0)
    expect(db.rows('readerReceipts')).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ targetRecordingId: 'sample', producerIncarnation, operationId: 'g1' })
      ])
    )
  } finally {
    resume?.()
    jest.useRealTimers()
  }
})

test('abandoning the caller wait does not release a live partition owner', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false,
    resume,
    cancel
  const locks = {
    request(_name, _options, callback) {
      if (held) return Promise.resolve(callback(null))
      held = true
      return Promise.resolve(callback({})).finally(() => {
        held = false
      })
    }
  }
  let waitOnce = true
  const firstDatabase = {
    transaction: db.transaction.bind(db),
    ready() {
      if (!waitOnce) return Promise.resolve()
      waitOnce = false
      return new Promise(resolve => {
        resume = resolve
      })
    }
  }
  const first = await createWebContinuationRecordingController({ database: firstDatabase, locks })
  const second = await createWebContinuationRecordingController({
    database: { transaction: db.transaction.bind(db) },
    locks
  })
  const work = first.clear('sample')
  const cancelled = new Promise(resolve => {
    cancel = () => resolve('cancelled')
  })
  for (let i = 0; i < 100 && !resume; i++) await Promise.resolve()
  expect(resume).toBeDefined()
  cancel()
  expect(await Promise.race([work, cancelled])).toBe('cancelled')
  await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  resume()
  expect(await work).toEqual({ cleared: true, records: 0 })
})

test('a returned clear retains its exact receipt until explicit acknowledgement', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = {
    request(_name, _options, callback) {
      return Promise.resolve(callback({}))
    }
  }
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
  expect(db.rows('readerReceipts')).toEqual(
    expect.arrayContaining([expect.objectContaining({ targetRecordingId: 'sample', operationId: 'g1' })])
  )
})

test.each(['allocation', 'deletion'])(
  'stolen lock fences the former clear during %s while a new owner runs',
  async phase => {
    const { db } = await sealedBinding()
    const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
    let owner,
      lateCompletion,
      transactionCount = 0
    const locks = {
      request(_name, _options, callback) {
        if (owner) return Promise.resolve(callback(null))
        let reject
        const stolen = new Promise((_resolve, fail) => {
          reject = fail
        })
        const hold = callback({})
        const entry = {
          steal: () => {
            owner = undefined
            reject(new Error('stolen'))
          }
        }
        owner = entry
        return Promise.race([hold, stolen]).finally(() => {
          if (owner === entry) owner = undefined
        })
      }
    }
    const database = {
      transaction(...args) {
        transactionCount++
        const tx = db.transaction(...args),
          original = tx.objectStore.bind(tx)
        let target = false
        tx.objectStore = name => {
          const store = original(name)
          if (name === 'readerReceipts' && phase === 'allocation') {
            const put = store.put
            store.put = value => {
              if (value.recordingId === ':ubm-clear-global-sequence-v1') target = true
              return put(value)
            }
          }
          if (name === 'recordings' && phase === 'deletion') {
            const remove = store.delete
            store.delete = key => {
              target = true
              return remove(key)
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
            if (target) lateCompletion = complete
            else complete?.()
          }
        })
        return tx
      }
    }
    const first = await createWebContinuationRecordingController({ database, locks })
    const second = await createWebContinuationRecordingController({ database: db, locks })
    jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
    try {
      const pending = first.clear('sample').catch(error => error)
      for (let i = 0; i < 100 && !lateCompletion; i++) await jest.advanceTimersByTimeAsync(0)
      expect(lateCompletion).toBeDefined()
      owner.steal()
      await jest.advanceTimersByTimeAsync(0)
      const before = transactionCount
      if (phase === 'allocation') {
        expect(await second.clear('sample')).toEqual({ cleared: true, records: 0 })
      } else {
        await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
      }
      lateCompletion()
      expect(await pending).toMatchObject({ code: 'capability.unavailable' })
      expect(transactionCount).toBe(before)
      expect(db.rows('readerReceipts')).toEqual(
        expect.arrayContaining([expect.objectContaining({ version: 3, targetRecordingId: 'sample', records: 0 })])
      )
    } finally {
      lateCompletion?.()
      jest.useRealTimers()
    }
  }
)

test('lost deletion and reconciliation completions retain bounded admission until both settle', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false,
    deletionLost = false,
    transactions = 0
  const completions = []
  const locks = {
    request(_name, _options, callback) {
      if (held) return Promise.resolve(callback(null))
      held = true
      return Promise.resolve(callback({})).finally(() => {
        held = false
      })
    }
  }
  const database = {
    transaction(...args) {
      transactions++
      const tx = db.transaction(...args),
        original = tx.objectStore.bind(tx)
      let deletion = false,
        reconciliation = false
      tx.objectStore = name => {
        const store = original(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            deletion = true
            return remove(key)
          }
        }
        if (name === 'readerReceipts') {
          const get = store.get
          store.get = key => {
            if (deletionLost && typeof key === 'string' && key.startsWith(':ubm-clear-v4:')) reconciliation = true
            return get(key)
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
          if (deletion || reconciliation) {
            deletionLost = true
            completions.push(complete)
          } else complete?.()
        }
      })
      return tx
    }
  }
  const first = await createWebContinuationRecordingController({ database, locks })
  const second = await createWebContinuationRecordingController({ database: db, locks })
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const pending = first.clear('sample').catch(error => error)
    for (let i = 0; i < 100 && completions.length < 1; i++) await jest.advanceTimersByTimeAsync(0)
    expect(completions).toHaveLength(1)
    await jest.advanceTimersByTimeAsync(5001)
    for (let i = 0; i < 100 && completions.length < 2; i++) await jest.advanceTimersByTimeAsync(0)
    expect(completions).toHaveLength(2)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await pending).toMatchObject({ code: 'capability.unavailable' })
    const before = transactions
    await expect(second.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
    expect(transactions).toBe(before)
    completions.forEach(complete => complete())
    for (let i = 0; i < 100 && held; i++) await jest.advanceTimersByTimeAsync(0)
    expect(held).toBe(false)
    expect(db.rows('readerReceipts')).toEqual(
      expect.arrayContaining([expect.objectContaining({ version: 3, targetRecordingId: 'sample' })])
    )
  } finally {
    completions.forEach(complete => complete())
    jest.useRealTimers()
  }
})

test('public clear observes coordinator failure while releasing after a committed deletion', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = {
    async request(_name, _options, callback) {
      await callback({})
      throw new Error('release failed')
    }
  }
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  await expect(controller.clear('sample')).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(db.rows('readerReceipts')).toEqual(
    expect.arrayContaining([expect.objectContaining({ version: 3, targetRecordingId: 'sample' })])
  )
})

test('public clear bounds an unconfirmed coordinator release after deletion commits', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let releaseRequested = false
  const locks = {
    request(_name, _options, callback) {
      Promise.resolve(callback({})).then(() => {
        releaseRequested = true
      })
      return new Promise(() => {})
    }
  }
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  jest.useFakeTimers({ doNotFake: ['queueMicrotask'] })
  try {
    const result = controller.clear('sample').catch(error => error)
    for (let i = 0; i < 100 && !releaseRequested; i++) await jest.advanceTimersByTimeAsync(0)
    expect(releaseRequested).toBe(true)
    await jest.advanceTimersByTimeAsync(5001)
    expect(await result).toMatchObject({ code: 'capability.unavailable' })
    expect(db.rows('readerReceipts')).toEqual(
      expect.arrayContaining([expect.objectContaining({ version: 3, targetRecordingId: 'sample' })])
    )
  } finally {
    jest.useRealTimers()
  }
})

test('public clear preserves both a primary failure and coordinator release failure', async () => {
  const { db } = await sealedBinding()
  db.remove('readerReceipts')
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = {
    async request(_name, _options, callback) {
      await callback({})
      throw new Error('release failed')
    }
  }
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  const error = await controller.clear('sample').catch(problem => problem)
  expect(error).toBeInstanceOf(AggregateError)
  expect(error.errors).toHaveLength(2)
})

test('public prepared clear attempts have distinct identities and retain exact outcomes after acknowledgement', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const locks = {
    request(_name, _options, callback) {
      return Promise.resolve(callback({}))
    }
  }
  const partition = createWebRecordingPartition({
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'unused'
  })
  const first = await partition.createController()
  const second = await partition.createController()
  const inspector = await createWebRecordingRecoveryInspector(partition)
  const attemptA = await first.prepareClearAttemptV1('sample')
  const attemptB = await second.prepareClearAttemptV1('sample')
  expect(Object.isFrozen(attemptA)).toBe(true)
  expect(attemptA.producerIncarnation).toBe(db.rows('recordings')[0].producerIncarnation)
  expect(attemptA.attemptId).not.toBe(attemptB.attemptId)
  expect(await first.clearAttemptV1(attemptA)).toEqual({ cleared: true, records: 0 })
  expect(await inspector.getClearAttemptV1(attemptB)).toEqual({ outcome: 'unknown' })
  await expect(second.clearAttemptV1(attemptB)).rejects.toMatchObject({ code: 'capability.unavailable' })
  expect(await inspector.getClearAttemptV1(attemptB)).toEqual({ outcome: 'superseded', attempt: attemptB })
  const observed = await inspector.getClearAttemptV1(attemptA)
  expect(observed).toMatchObject({ outcome: 'committed', receipt: { attemptId: attemptA.attemptId } })
  await inspector.acknowledgeClearAttemptV1(attemptA, observed.receipt)
  expect(await inspector.getClearAttemptV1(attemptA)).toMatchObject({ outcome: 'committed', receipt: observed.receipt })
})

test('V2 clear records its exact outcome and fences retries after explicit retirement across controllers', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const first = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const second = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'c'.repeat(64) })
  const identity = { recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: 'b'.repeat(64) }
  const attempt = await first.admitClearAttemptV2(identity)
  expect(await second.admitClearAttemptV2(identity)).toEqual(attempt)
  expect(await first.commitClearAttemptV2(attempt)).toEqual({ cleared: true, records: 0 })
  const outcome = await second.inspectClearAttemptV2(attempt)
  expect(outcome).toEqual({ state: 'committed', attempt, result: { cleared: true, records: 0 } })
  expect(await second.commitClearAttemptV2(attempt)).toEqual(outcome.result)
  expect(await first.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired' })
  expect(await second.admitClearAttemptV2(identity)).toEqual(attempt)
  await expect(second.commitClearAttemptV2(attempt)).rejects.toThrow()
  expect(await second.inspectClearAttemptV2(attempt)).toEqual({ state: 'retired-or-unknown' })
})

test('V2 deletion abort rolls back the committed proof and retains a retryable pending attempt', async () => {
  const { db, store } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const attempt = await controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: 'd'.repeat(64) })
  const transaction = db.transaction.bind(db)
  let injected = false
  db.transaction = (...args) => {
    const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
    tx.objectStore = name => {
      const object = objectStore(name)
      if (name === 'recordings') {
        const remove = object.delete
        object.delete = key => {
          if (!injected) { injected = true; tx.abort() }
          return remove(key)
        }
      }
      return object
    }
    return tx
  }
  await expect(controller.commitClearAttemptV2(attempt)).rejects.toThrow()
  expect(injected).toBe(true)
  expect(db.rows('recordings')).toHaveLength(1)
  expect(await store.read('sample')).toEqual(peer)
  expect(await controller.inspectClearAttemptV2(attempt)).toEqual({ state: 'pending', attempt })
  expect(await controller.commitClearAttemptV2(attempt)).toEqual({ cleared: true, records: 0 })
})

test('V2 committed proof survives a lost deletion reply and is inspectable from another controller', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const first = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const second = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'c'.repeat(64) })
  const attempt = await first.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: 'e'.repeat(64) })
  const transaction = db.transaction.bind(db)
  let lost = false
  db.transaction = (...args) => {
    const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
    let clearing = false, complete
    tx.objectStore = name => {
      const object = objectStore(name)
      if (name === 'recordings') {
        const remove = object.delete
        object.delete = key => { clearing = true; return remove(key) }
      }
      return object
    }
    Object.defineProperty(tx, 'oncomplete', {
      set: value => { complete = value },
      get: () => () => { if (clearing && !lost) { lost = true; tx.onabort() } else complete?.() }
    })
    return tx
  }
  expect(await first.commitClearAttemptV2(attempt)).toEqual({ cleared: true, records: 0 })
  expect(lost).toBe(true)
  expect(await second.inspectClearAttemptV2(attempt)).toEqual({ state: 'committed', attempt, result: { cleared: true, records: 0 } })
  expect(await second.commitClearAttemptV2(attempt)).toEqual({ cleared: true, records: 0 })
})

test('V2 retirement cannot pass a clear whose completion is still unconfirmed', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false, deliverAbort
  const locks = { request(_name, _options, callback) {
    if (held) return Promise.resolve(callback(null))
    held = true
    return Promise.resolve(callback({})).finally(() => { held = false })
  } }
  const transaction = db.transaction.bind(db)
  const database = { transaction(...args) {
    const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
    let deletion = false, complete
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'recordings') {
        const remove = store.delete
        store.delete = key => { deletion = true; return remove(key) }
      }
      return store
    }
    Object.defineProperty(tx, 'oncomplete', {
      set: value => { complete = value },
      get: () => () => { if (deletion) deliverAbort = () => tx.onabort(); else complete?.() }
    })
    return tx
  } }
  const first = await createWebContinuationRecordingController({ database, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const second = await createWebContinuationRecordingController({ database, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const attempt = await first.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: 'f'.repeat(64) })
  const pending = first.commitClearAttemptV2(attempt)
  for (let i = 0; i < 100 && !deliverAbort; i++) await Promise.resolve()
  expect(deliverAbort).toBeDefined()
  const outcome = await second.inspectClearAttemptV2(attempt)
  expect(outcome.state).toBe('committed')
  await expect(second.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).rejects.toThrow()
  deliverAbort()
  expect(await pending).toEqual({ cleared: true, records: 0 })
  expect(await second.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired' })
})

test('V2 admission snapshots caller identity at invocation', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const controller = await createWebContinuationRecordingController({ database: db, locks: admissionLocks(), randomToken: () => 'a'.repeat(64) })
  const input = { recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: '1'.repeat(64) }
  const pending = controller.admitClearAttemptV2(input)
  input.requestId = '2'.repeat(64)
  expect((await pending).requestId).toBe('1'.repeat(64))
})

test('a cancelled V2 attempt cannot delete a recording and needs explicit retirement', async () => {
  const { db, store } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const attempt = await controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: '3'.repeat(64) })
  const outcome = await controller.cancelClearAttemptV2(attempt)
  expect(outcome).toEqual({ state: 'cancelled', attempt })
  await expect(controller.commitClearAttemptV2(attempt)).rejects.toThrow()
  expect(db.rows('recordings')).toHaveLength(1)
  expect(await store.read('sample')).toEqual(peer)
  expect((await controller.clearProofUsageV2()).retained).toBe(1)
  await controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })
  expect((await controller.clearProofUsageV2()).retained).toBe(0)
})

test('a lost V2 retirement completion times out while retaining its lock until settlement', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false, deliver
  const locks = { request(_name, _options, callback) {
    if (held) return Promise.resolve(callback(null))
    held = true
    return Promise.resolve(callback({})).finally(() => { held = false })
  } }
  const controller = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const attempt = await controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: '4'.repeat(64) })
  const outcome = await controller.cancelClearAttemptV2(attempt)
  const transaction = db.transaction.bind(db)
  db.transaction = (...args) => {
    const tx = transaction(...args), objectStore = tx.objectStore.bind(tx)
    let retirement = false, complete
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'readerReceipts') {
        const remove = store.delete
        store.delete = key => { if (key.startsWith(':ubm-staged-proof-v1:')) retirement = true; return remove(key) }
      }
      return store
    }
    Object.defineProperty(tx, 'oncomplete', {
      set: value => { complete = value },
      get: () => () => { if (retirement && !deliver) deliver = () => complete(); else complete?.() }
    })
    return tx
  }
  jest.useFakeTimers()
  try {
    const pending = controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true }).catch(error => error)
    for (let i = 0; i < 100 && !deliver; i++) await Promise.resolve()
    expect(deliver).toBeDefined()
    await jest.advanceTimersByTimeAsync(5001)
    expect(await Promise.race([pending, Promise.resolve('unsettled')])).toMatchObject({ code: 'capability.unavailable' })
    await expect(controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).rejects.toThrow()
    deliver()
    for (let i = 0; i < 100 && held; i++) await Promise.resolve()
    expect(held).toBe(false)
    expect(await controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).toEqual({ state: 'retired-or-unknown' })
  } finally { if (held && deliver) deliver(); jest.useRealTimers() }
})

test('an invalid V2 retirement releases its clear lock', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  let held = false
  const locks = { request(_name, _options, callback) {
    if (held) return Promise.resolve(callback(null))
    held = true
    return Promise.resolve(callback({})).finally(() => { held = false })
  } }
  const controller = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  await expect(controller.retireClearAttemptV2({ outcome: { state: 'cancelled', attempt: {} }, acknowledgeDurableCopy: true })).rejects.toThrow()
  expect(held).toBe(false)
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
})

test('V2 recovery fails closed when its durable request identity fence is corrupt', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({ database: db, locks: withAdmissionLocks(locks), randomToken: () => 'a'.repeat(64) })
  const attempt = await controller.admitClearAttemptV2({ recordingId: 'sample', producerIncarnation: db.rows('recordings')[0].producerIncarnation, requestId: '5'.repeat(64) })
  await controller.commitClearAttemptV2(attempt)
  const outcome = await controller.inspectClearAttemptV2(attempt)
  db.mutate('readerReceipts', row => { if (row.recordingId.startsWith(':ubm-proof-request-v2:')) row.attempt.recordingId = 'other' })
  await expect(controller.inspectClearAttemptV2(attempt)).rejects.toThrow()
  await expect(controller.commitClearAttemptV2(attempt)).rejects.toThrow()
  await expect(controller.retireClearAttemptV2({ outcome, acknowledgeDurableCopy: true })).rejects.toThrow()
})

test('a full staged proof budget does not retire or block a V1 clear outcome', async () => {
  const { db } = await sealedBinding()
  const { createWebClearProofLedger } = require('../../src/web/web-clear-proof-ledger')
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const ledger = createWebClearProofLedger(db, () => 'a'.repeat(64))
  const usage = await ledger.usage()
  await ledger.configure({ limit: 1, expectedRevision: usage.revision })
  await ledger.admit({ recordingId: 'other', producerIncarnation: 'other', requestId: 'other' })
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({
    database: db,
    locks,
    randomToken: () => 'b'.repeat(64)
  })
  const attempt = await controller.prepareClearAttemptV1('sample')
  expect(await controller.commitClearAttemptV1(attempt)).toEqual({ cleared: true, records: 0 })
  const recovery = createWebRecordingRecovery(db)
  const outcome = await recovery.getClearAttemptV1(attempt)
  expect(outcome.outcome).toBe('committed')
  await recovery.acknowledgeClearAttemptV1(attempt, outcome.receipt)
  expect(await recovery.getClearAttemptV1(attempt)).toEqual(outcome)
  expect(await controller.commitClearAttemptV1(attempt)).toEqual({ cleared: true, records: 0 })
  expect((await ledger.usage()).retained).toBe(1)
})

test('a full staged proof budget does not change the default clear path', async () => {
  const { db } = await sealedBinding()
  const { createWebClearProofLedger } = require('../../src/web/web-clear-proof-ledger')
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const ledger = createWebClearProofLedger(db, () => 'a'.repeat(64))
  const usage = await ledger.usage()
  await ledger.configure({ limit: 1, expectedRevision: usage.revision })
  await ledger.admit({ recordingId: 'other', producerIncarnation: 'other', requestId: 'other' })
  const locks = { request: (_name, _options, callback) => Promise.resolve(callback({})) }
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  expect(await controller.clear('sample')).toEqual({ cleared: true, records: 0 })
  expect(db.rows('recordings')).toEqual([])
  expect((await ledger.usage()).retained).toBe(1)
})

test('prepared clear refuses a foreign incarnation and a cancelled commit without deleting the recording', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition } = require('../../src/web')
  const locks = {
    request(_name, _options, callback) {
      return Promise.resolve(callback({}))
    }
  }
  const partition = createWebRecordingPartition({
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'unused'
  })
  const controller = await partition.createController()
  const attempt = await controller.prepareClearAttemptV1('sample')
  await expect(controller.clearAttemptV1({ ...attempt, producerIncarnation: 'foreign' })).rejects.toThrow()
  const abort = new AbortController()
  abort.abort()
  await expect(controller.clearAttemptV1(attempt, { signal: abort.signal })).rejects.toMatchObject({
    code: 'operation.aborted'
  })
  expect(db.rows('recordings').find(row => row.id === 'sample')).toBeDefined()
})

test('prepared clear retains its exact outcome when lock release fails and permits an exact retry', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  let failRelease = true
  const locks = {
    async request(_name, _options, callback) {
      await callback({})
      if (failRelease) throw new Error('release failed')
    }
  }
  const partition = createWebRecordingPartition({
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'unused'
  })
  const controller = await partition.createController()
  const inspector = await createWebRecordingRecoveryInspector(partition)
  const attempt = await controller.prepareClearAttemptV1('sample')
  const originalRecording = db.rows('recordings')[0]
  await expect(controller.commitClearAttemptV1(attempt)).rejects.toMatchObject({ code: 'capability.unavailable' })
  const outcome = await inspector.getClearAttemptV1(attempt)
  expect(outcome).toMatchObject({ outcome: 'committed', receipt: { ...attempt, cleared: true, records: 0 } })
  await expect(inspector.getClearAttemptV1({ ...attempt, producerIncarnation: 'foreign' })).rejects.toThrow()
  await expect(inspector.acknowledgeClearAttemptV1(attempt, { ...outcome.receipt, records: 1 })).rejects.toThrow()
  db.seed('recordings', { ...originalRecording, producerIncarnation: 'successor' })
  failRelease = false
  expect(await controller.commitClearAttemptV1(attempt)).toEqual({ cleared: true, records: 0 })
  expect(db.rows('recordings')[0].producerIncarnation).toBe('successor')
})

test('prepared clear outcome survives coordinator loss after deletion commits', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  let steal, lateCompletion
  const locks = {
    request(_name, _options, callback) {
      const stolen = new Promise((_resolve, reject) => {
        steal = () => reject(new Error('stolen'))
      })
      return Promise.race([callback({}), stolen])
    }
  }
  const database = {
    transaction(...args) {
      const tx = db.transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let deletion = false,
        completion
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            deletion = true
            return remove(key)
          }
        }
        return store
      }
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          completion = fn
        },
        get: () => () => {
          if (deletion) lateCompletion = completion
          else completion?.()
        }
      })
      return tx
    }
  }
  const partition = createWebRecordingPartition({ database, locks: withAdmissionLocks(locks), crypto: webcrypto, digest: async () => 'unused' })
  const inspectorPartition = createWebRecordingPartition({
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'unused'
  })
  const controller = await partition.createController()
  const inspector = await createWebRecordingRecoveryInspector(inspectorPartition)
  const attempt = await controller.prepareClearAttemptV1('sample')
  const pending = controller.commitClearAttemptV1(attempt).catch(error => error)
  for (let i = 0; i < 100 && !lateCompletion; i++) await Promise.resolve()
  expect(lateCompletion).toBeDefined()
  steal()
  await Promise.resolve()
  const committed = await inspector.getClearAttemptV1(attempt)
  expect(committed).toMatchObject({ outcome: 'committed', receipt: attempt })
  await inspector.acknowledgeClearAttemptV1(attempt, committed.receipt)
  const receipt = db.rows('readerReceipts').find(row => row.version === 3 && row.targetRecordingId === 'sample')
  await inspector.disposeRetained('sample', attempt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt })
  lateCompletion()
  expect(await pending).toMatchObject({ code: 'capability.unavailable' })
  const afterReload = await createWebRecordingRecoveryInspector(inspectorPartition)
  expect(await afterReload.getClearAttemptV1(attempt)).toEqual(committed)
  expect(await afterReload.disposeRetained('sample', attempt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt, verifyRetiredClearReceipt: true })).toEqual({ state: 'more', deleted: 0, retiredReceipt: receipt })
})

test('cancellation while waiting for clear authority prevents allocation and deletion', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition } = require('../../src/web')
  let grant
  const locks = {
    request(_name, _options, callback) {
      return new Promise(resolve => {
        grant = () => resolve(callback({}))
      })
    }
  }
  const partition = createWebRecordingPartition({
    database: db,
    locks,
    crypto: webcrypto,
    digest: async () => 'unused'
  })
  const controller = await partition.createController()
  const attempt = await controller.prepareClearAttemptV1('sample')
  const abort = new AbortController()
  const pending = controller.commitClearAttemptV1(attempt, { signal: abort.signal })
  for (let i = 0; i < 100 && !grant; i++) await Promise.resolve()
  expect(grant).toBeDefined()
  let settled = false
  pending.catch(() => {
    settled = true
  })
  abort.abort()
  for (let i = 0; i < 100 && !settled; i++) await Promise.resolve()
  const settledBeforeGrant = settled
  grant()
  await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(settledBeforeGrant).toBe(true)
  expect(db.rows('recordings').find(row => row.id === 'sample')).toBeDefined()
  expect(db.rows('readerReceipts').find(row => row.recordingId === ':ubm-clear-global-sequence-v1')).toBeUndefined()
})

test('cancellation after allocation fences a delayed deletion transaction', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  let holdNext = false,
    resume
  const database = {
    ready() {
      if (!holdNext) return Promise.resolve()
      holdNext = false
      return new Promise(resolve => {
        resume = resolve
      })
    },
    transaction(...args) {
      const tx = db.transaction(...args),
        original = tx.objectStore.bind(tx)
      tx.objectStore = name => {
        const store = original(name)
        if (name === 'readerReceipts') {
          const put = store.put
          store.put = value => {
            if (value.recordingId === ':ubm-clear-global-sequence-v1') holdNext = true
            return put(value)
          }
        }
        return store
      }
      return tx
    }
  }
  const locks = {
    request(_name, _options, callback) {
      return Promise.resolve(callback({}))
    }
  }
  const partition = createWebRecordingPartition({ database, locks: withAdmissionLocks(locks), crypto: webcrypto, digest: async () => 'unused' })
  const controller = await partition.createController()
  const inspector = await createWebRecordingRecoveryInspector(partition)
  const attempt = await controller.prepareClearAttemptV1('sample')
  const abort = new AbortController()
  const pending = controller.commitClearAttemptV1(attempt, { signal: abort.signal })
  for (let i = 0; i < 100 && !resume; i++) await Promise.resolve()
  expect(resume).toBeDefined()
  abort.abort()
  resume()
  await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
  expect(db.rows('recordings').find(row => row.id === 'sample')).toBeDefined()
  expect(await inspector.getClearAttemptV1(attempt)).toEqual({ outcome: 'unknown' })
})

test('an inspector cannot retire a clear receipt before its original caller reconciles', async () => {
  const { db } = await sealedBinding()
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const { createWebRecordingRecovery } = require('../../src/web/web-recording-recovery')
  const locks = {
    request(_name, _options, callback) {
      return Promise.resolve(callback({}))
    }
  }
  let deliverAbort
  const database = {
    transaction(...args) {
      const tx = db.transaction(...args),
        objectStore = tx.objectStore.bind(tx)
      let deletion = false,
        completion
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => {
            deletion = true
            return remove(key)
          }
        }
        return store
      }
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => {
          completion = fn
        },
        get: () => () => {
          if (deletion) deliverAbort = () => tx.onabort()
          else completion?.()
        }
      })
      return tx
    }
  }
  const controller = await createWebContinuationRecordingController({ database, locks })
  const pending = controller.clear('sample')
  for (let i = 0; i < 100 && !deliverAbort; i++) await Promise.resolve()
  expect(deliverAbort).toBeDefined()
  const receipt = db.rows('readerReceipts').find(row => row.version === 3 && row.targetRecordingId === 'sample')
  expect(receipt).toBeDefined()
  const inspector = createWebRecordingRecovery(db)
  await inspector.disposeRetained('sample', receipt.producerIncarnation, {
    acknowledgeExport: true,
    limit: 1,
    clearReceipt: receipt
  })
  deliverAbort()
  await expect(pending).resolves.toEqual({ cleared: true, records: 0 })
})

test('a caller-known attempt survives both acknowledgements before delayed clear reconciliation', async () => {
  const { db } = await sealedBinding()
  const { webcrypto } = require('crypto')
  const { createWebRecordingPartition, createWebRecordingRecoveryInspector } = require('../../src/web')
  const locks = { request(_name, _options, callback) { return Promise.resolve(callback({})) } }
  let deliverAbort
  const database = {
    transaction(...args) {
      const tx = db.transaction(...args), objectStore = tx.objectStore.bind(tx)
      let deletion = false, completion
      tx.objectStore = name => {
        const store = objectStore(name)
        if (name === 'recordings') {
          const remove = store.delete
          store.delete = key => { deletion = true; return remove(key) }
        }
        return store
      }
      Object.defineProperty(tx, 'oncomplete', {
        set: fn => { completion = fn },
        get: () => () => { if (deletion) deliverAbort = () => tx.onabort(); else completion?.() }
      })
      return tx
    }
  }
  const partition = source => createWebRecordingPartition({ database: source, locks, crypto: webcrypto, digest: async () => 'unused' })
  const controller = await partition(database).createController()
  const inspector = await createWebRecordingRecoveryInspector(partition({ transaction: (...args) => db.transaction(...args) }))
  const attempt = await controller.prepareClearAttemptV1('sample')
  const pending = controller.commitClearAttemptV1(attempt)
  for (let i = 0; i < 100 && !deliverAbort; i++) await Promise.resolve()
  expect(deliverAbort).toBeDefined()
  const outcome = await inspector.getClearAttemptV1(attempt)
  expect(outcome.outcome).toBe('committed')
  await inspector.acknowledgeClearAttemptV1(attempt, outcome.receipt)
  const receipt = db.rows('readerReceipts').find(row => row.version === 3 && row.targetRecordingId === 'sample')
  await inspector.disposeRetained('sample', attempt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt })
  deliverAbort()
  await expect(pending).resolves.toEqual({ cleared: true, records: 0 })
  const reloaded = await createWebRecordingRecoveryInspector(partition(db))
  expect(await reloaded.getClearAttemptV1(attempt)).toEqual(outcome)
  expect(await reloaded.disposeRetained('sample', attempt.producerIncarnation, { acknowledgeExport: true, limit: 1, clearReceipt: receipt, verifyRetiredClearReceipt: true })).toEqual({ state: 'more', deleted: 0, retiredReceipt: receipt })
})

test('public clear redacts a host typed-error spoof when lock release also fails', async () => {
  const { db } = await sealedBinding()
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const spoof = Object.assign(Object.create(BackendContractError.prototype), {
    normalized: { code: 'stream.quota', domain: 'restoration', operation: 'web-recording.token-secret', retryability: 'never', platform: { domain: 'host-secret', code: 'code-secret', safeMessage: 'message-secret', metadata: {} } }
  })
  db.transaction = () => { throw spoof }
  const locks = { async request(_name, _options, callback) { await callback({}); throw new Error('release-secret') } }
  const { createWebContinuationRecordingController } = require('../../src/web/web-continuation-recording-controller')
  const controller = await createWebContinuationRecordingController({ database: db, locks })
  const error = await controller.clear('sample').catch(problem => problem)
  expect(error).toBeInstanceOf(AggregateError)
  expect(error.errors[0]).toMatchObject({ code: 'capability.unavailable', operation: 'web-recording.indexeddb-transaction' })
  expect(`${error.message} ${JSON.stringify(error.errors)}`).not.toContain('secret')
})

test('late observation completion still requires exact retirement proof for a direct bridge', async () => {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const { webRecordingBoundedPending } = require('../../src/web/web-indexeddb-transaction')
  const db = recordingDatabase()
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database: db, locks: { request: (_name, _options, fn) => Promise.resolve(fn({})) }, digest: async () => 'ab'.repeat(32) },
    input, webcrypto, { store: createWebRecordingPeerIdentityStore(db), reference: peer }
  )
  const timer = global.setTimeout
  let paused = false
  try {
    await bridge.prepare()
    await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
    await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
    await bridge.retire('1')
    await bridge.seal()
    db.pauseNext(); paused = true
    global.setTimeout = (callback, delay, ...args) => timer(callback, delay === 5000 ? 25 : delay, ...args)
    const timed = bridge.observeTerminal({ result: {
      recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] }
    } }).catch(error => error)
    await db.requestsProcessed()
    const pending = webRecordingBoundedPending(await timed)
    expect(pending).toBeDefined()
    db.complete(); paused = false
    await pending
    db.mutate('recordings', row => { row.terminalObservation.result.recording.state = 'interrupted' })
    await expect(bridge.releaseAuthority()).rejects.toMatchObject({ normalized: { operation: 'web-recording.terminal-unproven' } })
  } finally {
    if (paused) db.complete()
    global.setTimeout = timer
  }
})

test('a reconciled observation still requires exact retirement proof for a direct bridge', async () => {
  const { webcrypto } = require('crypto')
  const { createWebAuthorityRecordingJournalBridge } = require('../../src/web/web-authority-recording-journal-bridge')
  const db = recordingDatabase()
  const { producerIncarnation, memberEpochs, ...input } = declaration
  const bridge = createWebAuthorityRecordingJournalBridge(
    { database: db, locks: { request: (_name, _options, fn) => Promise.resolve(fn({})) }, digest: async () => 'ab'.repeat(32) },
    input, webcrypto, { store: createWebRecordingPeerIdentityStore(db), reference: peer }
  )
  const timer = global.setTimeout, transaction = db.transaction.bind(db)
  let deliver
  try {
    await bridge.prepare()
    await bridge.open({ memberships: () => [{ member: '0', epoch: '1' }] })
    await bridge.commit({ id: '1', record: { kind: 'terminal', reason: 'closed', uncertainTail: false } })
    await bridge.retire('1')
    await bridge.seal()
    db.transaction = (...args) => {
      const tx = transaction(...args)
      db.transaction = transaction
      let complete
      Object.defineProperty(tx, 'oncomplete', {
        set(fn) { complete = fn },
        get() { return () => { deliver = () => complete() } }
      })
      return tx
    }
    global.setTimeout = (callback, delay, ...args) => timer(callback, delay === 5000 ? 25 : delay, ...args)
    await expect(bridge.observeTerminal({ result: {
      recording: { version: 1, state: 'finished', error: null }, hostCleanup: { state: 'released', failures: [] }
    } })).rejects.toThrow()
    expect(deliver).toBeDefined()
    expect(await bridge.reconcileObservation()).toBe('committed')
    db.mutate('recordings', row => { row.terminalObservation.result.recording.state = 'interrupted' })
    await expect(bridge.releaseAuthority()).rejects.toMatchObject({ normalized: { operation: 'web-recording.terminal-unproven' } })
  } finally {
    deliver?.()
    global.setTimeout = timer
  }
})
