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

async function publicFixture() {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: locks(),
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

function injectAtRecordCommits(f, limit) {
  const original = f.storage.transaction.bind(f.storage)
  let injections = 0
  f.storage.transaction = (...args) => {
    const tx = original(...args)
    const objectStore = tx.objectStore.bind(tx)
    let writesRecord = false
    tx.objectStore = name => {
      const store = objectStore(name)
      if (name === 'records') {
        for (const method of ['put', 'add']) {
          const action = store[method]
          store[method] = value => {
            writesRecord = true
            return action(value)
          }
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
        complete?.()
        if (writesRecord && injections < limit) {
          injections++
          f.boundary.emitNotification({
            serviceUuid: f.boundary.serviceUuid,
            serviceOccurrence: 0,
            characteristicUuid: f.boundary.characteristicUuid,
            characteristicOccurrence: 0,
            value: Uint8Array.of(injections)
          })
        }
      }
    })
    return tx
  }
  return () => injections
}

test('public stop starts physical cleanup while callbacks arrive at every record commit', async () => {
  const f = await publicFixture()
  try {
    const start = f.database.reserveOrderedRecordingStart()
    expect(
      (
        await start.start([{ memberId: 'heart', characteristic: f.characteristic }], {
          recording: { id: 'stop-starvation', maxBytes: 1048576, maxRecords: 30 }
        })
      ).kind
    ).toBe('started')
    const counts = injectAtRecordCommits(f, 8)
    const physicalStop = f.boundary.characteristic.stopNotifications
    let injectionsAtStop
    f.boundary.characteristic.stopNotifications = () => {
      injectionsAtStop = counts()
      return physicalStop()
    }
    f.boundary.emitNotification({
      serviceUuid: f.boundary.serviceUuid,
      serviceOccurrence: 0,
      characteristicUuid: f.boundary.characteristicUuid,
      characteristicOccurrence: 0,
      value: Uint8Array.of(99)
    })
    await f.controller.stopOrdered('stop-starvation')
    expect(counts()).toBeGreaterThan(0)
    expect(injectionsAtStop).toBeLessThan(8)
    expect(injectionsAtStop).toBeLessThan(counts())
    expect(f.boundary.notificationStops).toBe(1)
    const batch = await f.controller.prepareOrdered('stop-starvation', { maxItems: 30, maxBytes: 10000 })
    expect(batch.records[0].record.kind).toBe('value')
    expect(batch.records.at(-1).record.kind).toBe('terminal')
    await f.controller.acknowledge('stop-starvation', batch.token)
  } finally {
    await f.manager.destroy()
  }
})

const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')

function deferred() {
  let resolve
  const promise = new Promise(done => {
    resolve = done
  })
  return { promise, resolve }
}

function supervisorFixture() {
  const ingress = new WebNotificationIngress()
  const target = {}
  const reservation = ingress.reserveRetainedMemberships(new Map([[target, '0']]), 4, 100, 2)
  const lease = { id: '0', enable: jest.fn(async () => {}), release: jest.fn(async () => {}) }
  let source
  const owner = {
    reserve: () => reservation,
    install: wake => {
      source = reservation.install(wake)
      return { source, leases: [lease] }
    },
    release: jest.fn(async () => {}),
    isCurrent: () => true
  }
  const journal = {
    prepare: async () => {},
    open: async () => {},
    commit: jest.fn(async () => {}),
    retire: jest.fn(async () => {}),
    seal: jest.fn(async () => {}),
    releaseAuthority: async () => {},
    classifyFailure: () => 'capacity',
    isDefinitiveFailure: () => true
  }
  const supervisor = new WebOrderedRecordingSupervisor(owner, journal)
  return {
    ingress,
    target,
    lease,
    owner,
    journal,
    supervisor,
    get source() {
      return source
    }
  }
}

async function until(predicate) {
  for (let i = 0; i < 200; i++) {
    if (predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error('condition did not settle')
}

test('drains callbacks during an in-flight stop but holds terminal until cleanup', async () => {
  const f = supervisorFixture()
  const cleanup = deferred()
  const commit = deferred()
  f.lease.release.mockReturnValueOnce(cleanup.promise)
  f.journal.commit.mockReturnValueOnce(commit.promise)
  await f.supervisor.start()
  f.supervisor.stop()
  const stopping = f.supervisor.retry()
  try {
    await until(() => f.lease.release.mock.calls.length === 1)
    f.ingress.admit(f.target, Uint8Array.of(1))
    await until(() => f.journal.commit.mock.calls.some(([d]) => d.record.kind === 'failure-observation'))
    f.ingress.admit(f.target, Uint8Array.of(2, 3))
    expect(f.source.peek().id).toBe('1')
    expect(f.journal.retire).not.toHaveBeenCalled()
    commit.resolve()
    await until(() => f.journal.commit.mock.calls.length === 2)
    expect(f.journal.commit.mock.calls.map(([d]) => d.record.sourceRejection.bytes)).toEqual(['1', '2'])
    expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).not.toContain('terminal')
    expect(f.journal.seal).not.toHaveBeenCalled()
  } finally {
    commit.resolve()
    cleanup.resolve()
    await stopping
  }
  expect(f.supervisor.status().state).toBe('finished')
  expect(f.journal.commit.mock.calls.map(([d]) => d.record.kind)).toEqual([
    'failure-observation',
    'failure-observation',
    'terminal'
  ])
})

test('capacity retains the exact head while cleanup fails, then retries both before terminal', async () => {
  const f = supervisorFixture()
  f.lease.release.mockRejectedValueOnce(new Error('stop failed'))
  f.journal.commit.mockRejectedValueOnce(new Error('full'))
  await f.supervisor.start()
  f.ingress.admit(f.target, Uint8Array.of(7))
  f.supervisor.stop()
  await f.supervisor.retry()
  const first = f.journal.commit.mock.calls[0][0]
  expect(f.source.peek()).toEqual(first)
  expect(f.supervisor.status()).toMatchObject({ state: 'waiting-capacity', terminalCommitted: false })
  expect(f.lease.release).toHaveBeenCalledTimes(1)
  expect(f.journal.seal).not.toHaveBeenCalled()
  await f.supervisor.retry()
  expect(f.lease.release).toHaveBeenCalledTimes(2)
  expect(f.journal.commit.mock.calls[1][0]).toEqual(first)
  expect(f.journal.commit.mock.calls.at(-1)[0].record.kind).toBe('terminal')
  expect(f.supervisor.status().state).toBe('finished')
})
