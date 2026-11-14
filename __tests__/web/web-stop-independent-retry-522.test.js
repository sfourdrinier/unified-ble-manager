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
const { awaitSignal } = require('../helpers/async')

function locks() {
  const held = new Set()
  let rejectLock
  return {
    lose() { rejectLock(new Error('coordinator lost')) },
    request(name, _options, callback) {
      if (held.has(name)) return Promise.resolve(callback(null))
      held.add(name)
      const lost = new Promise((_resolve, reject) => { rejectLock = reject })
      return Promise.race([Promise.resolve(callback({})), lost]).finally(() => held.delete(name))
    }
  }
}

async function publicFixture() {
  const boundary = new InMemoryWebBluetoothTckBoundary()
  const storage = transactionalDatabase()
  const liveOwners = createWebRecordingLiveOwners()
  const coordinator = locks()
  const install = liveOwners.install.bind(liveOwners)
  let owner
  liveOwners.install = (identity, value) => {
    owner = value
    install(identity, value)
  }
  const provider = createWebBluetoothProvider(boundary, {
    database: storage,
    locks: coordinator,
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
    {
      filters: [],
      acceptAllDevices: true,
      optionalServices: [boundary.serviceUuid, boundary.instances[1].serviceUuid]
    },
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
  return {
    boundary,
    coordinator,
    backend,
    storage,
    liveOwners,
    manager,
    connection,
    database,
    characteristic,
    controller,
    get owner() {
      return owner
    }
  }
}

function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

async function within(promise, description = 'stop result') {
  return awaitSignal(promise, description)
}

async function until(predicate, label) {
  for (let i = 0; i < 200; i++) {
    if (predicate()) return
    await new Promise(resolve => setTimeout(resolve, 0))
  }
  throw new Error(`condition did not settle: ${label}`)
}

function emit(f, size, instance = f.boundary.instances[0]) {
  f.boundary.emitNotification({
    serviceUuid: instance.serviceUuid,
    serviceOccurrence: instance.serviceOccurrence,
    characteristicUuid: instance.characteristicUuid,
    characteristicOccurrence: instance.characteristicOccurrence,
    value: new Uint8Array(size)
  })
}

const limits = { maxItems: 20, maxBytes: 20000 }

test('late storage success drains independent values while failed member cleanup remains owned', async () => {
  const f = await publicFixture()
  const gate = deferred()
  let stopCalls = 0
  const originalStop = f.boundary.characteristic.stopNotifications
  const originalTimer = global.setTimeout
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'late-storage-cleanup', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const commit = journal.commit.bind(journal)
    let entered = false
    journal.commit = async delivery => {
      if (!entered && delivery.record.kind === 'value') {
        entered = true
        await gate.promise
      }
      return commit(delivery)
    }
    f.boundary.characteristic.stopNotifications = async () => {
      if (++stopCalls === 1) throw new Error('private cleanup')
      return originalStop()
    }
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 30 : delay, ...args)
    emit(f, 1)
    emit(f, 2)
    await until(() => entered, 'first commit dispatched')
    expect(entered).toBe(true)
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().members[0].cleanup === 'failed', 'member cleanup failure')
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    expect(f.owner.supervisor.status().members[0].cleanup).toBe('failed')
    gate.resolve()
    await until(() => f.storage.rows('records').filter(row => row.record.kind === 'value').length === 3, 'independent retained value')
    expect(f.storage.rows('records').map(row => row.record.kind)).toEqual(['value', 'value', 'value'])
    expect(f.owner.supervisor.status()).toMatchObject({ authorityReleased: false, terminalCommitted: false, members: [{ cleanup: 'failed' }] })
    expect(stopCalls).toBe(1)
    expect(await f.controller.stopOrdered('late-storage-cleanup')).toMatchObject({ hostCleanup: { state: 'released' } })
  } finally {
    gate.resolve()
    global.setTimeout = originalTimer
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('public diagnostic records enable failure while its cleanup is pending', async () => {
  const f = await publicFixture()
  const gate = deferred()
  const originalStop = f.boundary.characteristic.stopNotifications
  f.boundary.characteristic.startNotifications = async () => { throw new DOMException('private-enable', 'NetworkError') }
  f.boundary.characteristic.stopNotifications = async () => { await gate.promise; return originalStop() }
  try {
    const result = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'pending-enable-cleanup', maxBytes: 1048576, maxRecords: 20 } }
    )
    expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'NetworkError' }] })
    expect(result).toMatchObject({ receipt: { state: 'setup-failed' } })
    const diagnostic = await f.controller.runtimeDiagnostic('pending-enable-cleanup')
    expect(diagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'connection.lost' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-enable')
  } finally {
    gate.resolve()
    await f.controller.stopOrdered('pending-enable-cleanup').catch(() => {})
    await f.manager.destroy()
  }
})

test.each(['untrusted', 'trusted', 'nested'])('unstarted installation cleanup retains its source and cleanup classification (%s)', async kind => {
  const { WebIngressRegistrationCleanupError } = require('../../src/web/web-notification-ingress')
  const { webRecordingContractError } = require('../../src/web/web-recording-errors')
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const installation = webRecordingContractError('connection.lost', 'connection', 'web-recording.install')
  const specific = webRecordingContractError('platform.failure', 'cleanup', 'web-recording.specific-release')
  const releaseError = kind === 'untrusted' ? new Error('private-release') : kind === 'nested'
    ? new WebIngressRegistrationCleanupError([installation, specific]) : specific
  const cleanup = new WebIngressRegistrationCleanupError([installation, releaseError])
  let releases = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    const release = owner.release.bind(owner)
    owner.install = () => { throw cleanup }
    owner.release = async () => { if (++releases === 1) throw cleanup; return release() }
    return owner
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'unstarted-release-failure', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => releases > 0, 'unstarted source release')
    expect(releases).toBe(1)
    expect(f.owner.supervisor.status()).toMatchObject({ sourceReleased: false, terminalCommitted: false, authorityReleased: false, failure: cleanup })
    const diagnostic = await f.controller.runtimeDiagnostic('unstarted-release-failure')
    expect(diagnostic).toMatchObject({ lastFailure: { error: { code: 'platform.failure', domain: 'cleanup', operation: kind === 'untrusted' ? 'web-recording.cleanup' : 'web-recording.specific-release' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-release')
    await f.controller.stopOrdered('unstarted-release-failure')
    expect(releases).toBe(2)
    expect(f.storage.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  } finally {
    if (f.owner) {
      f.owner.supervisor.journal.canRetryUnknown = () => true
      await f.owner.supervisor.retry().catch(() => {})
    }
    await f.manager.destroy()
  }
})

test.each([false, true])('nested registration cleanup cannot become the public setup failure (trusted installation: %s)', async trusted => {
  const { WebIngressRegistrationCleanupError } = require('../../src/web/web-notification-ingress')
  const { webRecordingContractError } = require('../../src/web/web-recording-errors')
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const nested = new WebIngressRegistrationCleanupError([
    trusted ? webRecordingContractError('connection.lost', 'connection', 'web-recording.install') : new Error('private-install'),
    webRecordingContractError('platform.failure', 'cleanup', 'web-recording.nested-cleanup')
  ])
  const failure = new WebIngressRegistrationCleanupError([nested, new Error('private-rollback')])
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    owner.install = () => { throw failure }
    return owner
  }
  try {
    const start = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'nested-install-failure', maxBytes: 1048576, maxRecords: 20 } }
    )
    expect(start).toMatchObject({ receipt: { error: { kind: 'source', source: {
      code: trusted ? 'connection.lost' : 'platform.failure',
      domain: trusted ? 'connection' : 'gatt',
      operation: trusted ? 'web-recording.install' : 'web-recording.setup'
    } } } })
    expect(JSON.stringify(start)).not.toContain('private-')
    await f.controller.stopOrdered('nested-install-failure')
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('unstarted journal finalization failure retains storage state after source release', async () => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const install = f.liveOwners.install.bind(f.liveOwners)
  const failure = new Error('private-finalize')
  let releases = 0, finalizations = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    const release = owner.release.bind(owner)
    owner.install = () => { throw new Error('private-install') }
    owner.release = async () => { releases++; return release() }
    return owner
  }
  f.liveOwners.install = (identity, value) => {
    install(identity, value)
    const journal = value.supervisor.journal
    const finalize = journal.finalizeUnstarted.bind(journal)
    const definitive = journal.isDefinitiveFailure.bind(journal)
    journal.isDefinitiveFailure = error => error === failure || definitive(error)
    journal.finalizeUnstarted = (...args) => {
      if (++finalizations === 1) return Promise.reject(failure)
      return finalize(...args)
    }
  }
  try {
    const start = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'unstarted-finalization-failure', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => finalizations === 1, 'journal finalization failure')
    expect(f.owner.supervisor.status()).toMatchObject({ sourceReleased: true, cleanupFailure: null, terminalCommitted: false, authorityReleased: false })
    expect(releases).toBe(1)
    const diagnostic = await f.controller.runtimeDiagnostic('unstarted-finalization-failure')
    expect(diagnostic).toMatchObject({ lastFailure: { error: { domain: 'restoration', operation: 'web-recording.indexeddb-transaction' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-finalize')
    const stop = await f.controller.stopOrdered('unstarted-finalization-failure')
    expect(f.owner.supervisor.status()).toMatchObject({ sealed: true, sourceReleased: true, authorityReleased: true })
    expect(start).toMatchObject({ receipt: { error: { kind: 'source', source: { code: 'platform.failure', domain: 'gatt', operation: 'web-recording.setup' } } } })
    expect(stop).toMatchObject({ recording: { state: 'failed', error: { kind: 'source', source: { domain: 'gatt', operation: 'web-recording.setup' } } }, hostCleanup: { state: 'released' } })
    expect(finalizations).toBe(2)
    expect(releases).toBe(1)
    expect(f.storage.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('public recording recovers storage after overlapping member cleanup failure', async () => {
  const { WebRecordingReadinessError } = require('../../src/web/web-indexeddb-transaction')
  const f = await publicFixture()
  const commit = deferred(), cleanup = deferred()
  let stopping
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'overlapping-failures', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const originalCommit = journal.commit.bind(journal)
    const classify = journal.classifyFailure.bind(journal)
    const definitive = journal.isDefinitiveFailure.bind(journal)
    let first = true
    journal.commit = delivery => {
      if (first && delivery.record.kind === 'value') { first = false; return commit.promise }
      return originalCommit(delivery)
    }
    journal.classifyFailure = error => { cleanup.reject(new Error('private-stop-failure')); return classify(error) }
    journal.isDefinitiveFailure = error => error instanceof WebRecordingReadinessError || definitive(error)
    const originalStop = f.boundary.characteristic.stopNotifications
    let stopCalls = 0
    f.boundary.characteristic.stopNotifications = async () => {
      if (++stopCalls === 1) await cleanup.promise
      return originalStop()
    }
    emit(f, 3)
    stopping = f.controller.stopOrdered('overlapping-failures')
    await until(() => !first && stopCalls === 1, 'commit and cleanup dispatched')
    commit.reject(new WebRecordingReadinessError(new Error('temporary storage failure')))
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'cleanup failure')
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    const diagnostic = await f.controller.runtimeDiagnostic('overlapping-failures')
    expect(diagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { domain: 'cleanup' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-stop-failure')
    await stopping
    await until(() => f.owner.supervisor.status().state === 'finished', 'autonomous drain')
    await until(() => !f.liveOwners.has({ recordingId: 'overlapping-failures', producerIncarnation: f.storage.rows('recordings')[0].producerIncarnation, ownerEpoch: f.storage.rows('recordings')[0].ownerEpoch }), 'owner retirement')
    const batch = await f.controller.prepareOrdered('overlapping-failures', limits)
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'value', 'terminal'])
    expect(Array.from(batch.records[1].record.bytes)).toEqual([0, 0, 0])
    expect(stopCalls).toBe(2)
  } finally {
    cleanup.resolve()
    commit.resolve()
    await stopping?.catch(() => {})
    await f.manager.destroy()
  }
})

test.each(['source', 'authority'])('public diagnostic retains %s cleanup failure during its retry', async phase => {
  const f = await publicFixture()
  const gate = deferred()
  let retry
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: `cleanup-${phase}`, maxBytes: 1048576, maxRecords: 20 } }
    )
    const target = phase === 'source' ? f.owner.supervisor.owner : f.owner.supervisor.journal
    const method = phase === 'source' ? 'release' : 'releaseAuthority'
    const original = target[method].bind(target)
    let calls = 0
    target[method] = async () => {
      if (++calls === 1) throw new Error(`private-${phase}-failure`)
      await gate.promise
      return original()
    }
    await f.controller.stopOrdered(`cleanup-${phase}`)
    const first = await f.controller.runtimeDiagnostic(`cleanup-${phase}`)
    expect(first).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'web-recording.cleanup' } } })
    expect(JSON.stringify(first)).not.toContain(`private-${phase}-failure`)
    retry = f.controller.stopOrdered(`cleanup-${phase}`)
    await until(() => calls === 2, 'cleanup retry dispatched')
    expect((await f.controller.runtimeDiagnostic(`cleanup-${phase}`)).lastFailure).toEqual(first.lastFailure)
    gate.resolve()
    expect(await retry).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
  } finally {
    gate.resolve()
    await retry?.catch(() => {})
    await f.manager.destroy()
  }
})

test('public diagnostic retains a failed source detach while storage is blocked', async () => {
  const f = await publicFixture()
  const gate = deferred()
  let retry
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'cleanup-detach', maxBytes: 1048576, maxRecords: 20 } }
    )
    const supervisor = f.owner.supervisor
    const journal = supervisor.journal, owner = supervisor.owner
    const commit = journal.commit.bind(journal), definitive = journal.isDefinitiveFailure.bind(journal)
    const detach = owner.releaseRetainingDeliveries.bind(owner)
    const storageFailure = new Error('storage blocked')
    let first = true, calls = 0
    journal.isDefinitiveFailure = error => error === storageFailure || definitive(error)
    journal.commit = delivery => {
      if (first && delivery.record.kind === 'value') { first = false; return Promise.reject(storageFailure) }
      return commit(delivery)
    }
    owner.releaseRetainingDeliveries = async () => {
      if (++calls === 1) throw new Error('private-detach-failure')
      await gate.promise
      return detach()
    }
    emit(f, 3)
    await until(() => calls === 1, 'failed source detach')
    const firstDiagnostic = await f.controller.runtimeDiagnostic('cleanup-detach')
    expect(firstDiagnostic).toMatchObject({ state: 'failed', lastFailure: { persisted: false, error: { code: 'platform.failure', domain: 'cleanup', operation: 'web-recording.cleanup' } } })
    expect(JSON.stringify(firstDiagnostic)).not.toContain('private-detach-failure')
    retry = f.controller.stopOrdered('cleanup-detach')
    await until(() => calls === 2 || supervisor.status().authorityReleased, 'source detach retry')
    if (calls === 2) {
      expect((await f.controller.runtimeDiagnostic('cleanup-detach')).lastFailure).toEqual(firstDiagnostic.lastFailure)
    }
    gate.resolve()
    await retry
  } finally {
    gate.resolve()
    await retry?.catch(() => {})
    await f.manager.destroy()
  }
})

test('repeated public stop retries a failed member without reader acknowledgement or duplicating a held release', async () => {
  const f = await publicFixture()
  const firstHeld = deferred()
  const retryHeld = deferred()
  const secondInstance = f.boundary.instances[1]
  const second = f.database.characteristic(secondInstance.serviceUuid, secondInstance.characteristicUuid, {
    serviceOccurrence: 0,
    characteristicOccurrence: 0
  })
  const firstOriginal = f.boundary.characteristic.stopNotifications
  const secondOriginal = secondInstance.characteristic.stopNotifications
  const failure = new Error('transient second stop failure')
  let firstCalls = 0
  let secondCalls = 0
  f.boundary.characteristic.stopNotifications = async () => {
    firstCalls++
    await firstHeld.promise
    return firstOriginal()
  }
  secondInstance.characteristic.stopNotifications = async () => {
    secondCalls++
    if (secondCalls === 1) throw failure
    await retryHeld.promise
    return secondOriginal()
  }
  let stopping, retrying, joining
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [
        { memberId: 'heart', characteristic: f.characteristic },
        { memberId: 'battery', characteristic: second }
      ],
      { recording: { id: 'independent-retry', maxBytes: 1048576, maxRecords: 20 } }
    )
    stopping = f.controller.stopOrdered('independent-retry')
    await until(() => f.owner.supervisor.status().members.some(member => member.cleanup === 'failed'), 'failed release')
    expectConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] })
    emit(f, 7)
    await until(() => f.storage.rows('records').some(row => row.record.sourceRejection?.bytes === '7'), 'retained callback')
    retrying = f.controller.stopOrdered('independent-retry')
    await until(() => secondCalls === 2, 'retry without reader acknowledgement')
    joining = f.controller.stopOrdered('independent-retry')
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(firstCalls).toBe(1)
    expect(secondCalls).toBe(2)
    retryHeld.resolve()
    await until(
      () => f.owner.supervisor.status().members.find(member => member.id === 'battery')?.cleanup === 'released',
      'retry release completed while first is held'
    )
    expect(f.owner.supervisor.status()).toMatchObject({ terminalCommitted: false, sealed: false, authorityReleased: false })
    expect(firstCalls).toBe(1)
    firstHeld.resolve()
    for (const result of await Promise.all([stopping, retrying, joining])) {
      expect(result).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    }
    const batch = await f.controller.prepareOrdered('independent-retry', limits)
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'failure-observation', 'terminal'])
    expect(batch.records[1].record.sourceRejection.bytes).toBe('7')
    expect(batch.records.map(row => Number(row.metadata.deliveryId))).toEqual([1, 2, 3])
    expect(firstCalls).toBe(1)
    expect(secondCalls).toBe(2)
    await f.controller.acknowledge('independent-retry', batch.token)
  } finally {
    firstHeld.resolve()
    retryHeld.resolve()
    await Promise.allSettled([stopping, retrying, joining].filter(Boolean))
    await f.manager.destroy()
  }
})

test('a repeated public stop reports its own failed retry while another release is held', async () => {
  const f = await publicFixture()
  const held = deferred()
  const retryHeld = deferred()
  const other = f.boundary.instances[1]
  const second = f.database.characteristic(other.serviceUuid, other.characteristicUuid, {
    serviceOccurrence: 0,
    characteristicOccurrence: 0
  })
  const firstOriginal = f.boundary.characteristic.stopNotifications
  const secondOriginal = other.characteristic.stopNotifications
  const failures = [new Error('first release failed'), new Error('retry release failed')]
  const firstFailure = deferred()
  const retryDispatched = deferred()
  const retryFailure = deferred()
  const thirdRelease = deferred()
  let firstCalls = 0
  let secondCalls = 0
  f.boundary.characteristic.stopNotifications = async () => {
    firstCalls++
    await held.promise
    return firstOriginal()
  }
  other.characteristic.stopNotifications = async () => {
    const attempt = secondCalls++
    if (attempt === 0) {
      firstFailure.resolve()
      throw failures[attempt]
    }
    if (attempt === 1) {
      retryDispatched.resolve()
      await retryHeld.promise
      retryFailure.resolve()
      throw failures[attempt]
    }
    thirdRelease.resolve()
    return secondOriginal()
  }
  let first, secondStop, joiningRetry, third
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }, { memberId: 'battery', characteristic: second }],
      { recording: { id: 'failed-retry-receipt', maxBytes: 1048576, maxRecords: 20 } }
    )
    first = f.controller.stopOrdered('failed-retry-receipt')
    await awaitSignal(firstFailure.promise, 'first member cleanup failure')
    await awaitSignal(
      waitForConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] }),
      'first member cleanup diagnostic'
    )
    secondStop = f.controller.stopOrdered('failed-retry-receipt')
    await awaitSignal(retryDispatched.promise, 'member cleanup retry dispatch')
    joiningRetry = f.controller.stopOrdered('failed-retry-receipt')
    retryHeld.resolve()
    await awaitSignal(retryFailure.promise, 'retry member cleanup failure')
    await awaitSignal(
      waitForConsoleError('[WebBluetoothGattRuntime.stopManagedSubscription] Notification stop rejected:', { errors: [{ name: 'Error' }] }),
      'retry member cleanup diagnostic'
    )
    const retryResult = await within(secondStop, 'failed retry stop result')
    expect(retryResult).toMatchObject({ hostCleanup: { state: 'release-failed' } })
    expect(await within(joiningRetry, 'joining failed retry stop result')).toMatchObject({
      hostCleanup: { state: 'release-failed' }
    })
    third = f.controller.stopOrdered('failed-retry-receipt')
    await awaitSignal(thirdRelease.promise, 'third member cleanup release')
    expect(firstCalls).toBe(1)
    held.resolve()
    for (const result of await Promise.all([first, third])) {
      expect(result).toMatchObject({ recording: { state: 'finished' }, hostCleanup: { state: 'released' } })
    }
  } finally {
    held.resolve()
    retryHeld.resolve()
    await Promise.allSettled([first, secondStop, joiningRetry, third].filter(Boolean))
    await f.manager.destroy()
  }
})


test('public stop retries real failed ingress rollback and releases authority', async () => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  let removes = 0
  const installFailure = new Error('private-install-boundary')
  const cleanupFailures = [new Error('private-rollback-1'), new Error('private-rollback-2')]
  let ingress
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    ingress = owner.ingress
    const changed = ingress.membershipChanged
    ingress.membershipChanged = (characteristic, added, registration) => {
      if (added) { changed(characteristic, true, registration); throw installFailure }
      removes++
      if (removes <= 2) throw cleanupFailures[removes - 1]
      changed(characteristic, false, registration)
    }
    return owner
  }
  try {
    const start = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'real-rollback-retry', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => removes >= 2, 'failed owner rollback')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ sourceReleased: false, authorityReleased: false })
    expect(status.setupFailure.errors).toEqual([installFailure, cleanupFailures[0]])
    expect(status.cleanupFailure.errors).toEqual(expect.arrayContaining([installFailure, ...cleanupFailures]))
    const stop = await f.controller.stopOrdered('real-rollback-retry')
    expect(removes).toBe(3)
    expect(start).toMatchObject({ receipt: { error: { kind: 'source', source: { domain: 'gatt', operation: 'web-recording.setup' } } } })
    expect(stop).toMatchObject({ recording: { state: 'failed', error: { kind: 'source', source: { domain: 'gatt', operation: 'web-recording.setup' } } }, hostCleanup: { state: 'released' } })
    expect(f.owner.supervisor.status()).toMatchObject({ sealed: true, sourceReleased: true, authorityReleased: true })
    expect(f.storage.rows('records').map(row => row.record.kind)).toEqual(['terminal'])
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('retrying a real removal after its effect preserves another registration', async () => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const characteristic = f.boundary.characteristic
  const memberships = f.backend.gattRuntime.ingressMemberships
  let other, removes = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    const ingress = owner.ingress
    other = ingress.register(new Map([[characteristic, 'other']]), 4, 16)
    const changed = ingress.membershipChanged
    ingress.membershipChanged = (member, added, registration) => {
      changed(member, added, registration)
      if (added) throw new Error('private-install')
      if (++removes === 1) throw new Error('private-after-removal')
    }
    return owner
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'effectful-removal', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => removes >= 2, 'membership removal retry')
    const registrations = [...memberships.get(characteristic).values()][0]
    expect(registrations.size).toBe(1)
    expect(await f.controller.stopOrdered('effectful-removal')).toMatchObject({ hostCleanup: { state: 'released' } })
    f.owner.supervisor.owner.ingress.admit(characteristic, new Uint8Array([7]))
    expect(other.drain()).toEqual([{ kind: 'value', memberId: 'other', ordinal: '1', value: new Uint8Array([7]) }])
    expect(registrations.size).toBe(1)
  } finally {
    other?.retire('closed')
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test.each([false, true])('late terminal observation reports unknown storage with earlier setup failure %s', async setupFails => {
  const f = await publicFixture()
  const gate = deferred()
  const originalTimer = global.setTimeout
  let observations = 0
  const install = f.liveOwners.install.bind(f.liveOwners)
  f.liveOwners.install = (identity, owner) => {
    install(identity, owner)
    const journal = owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = async observation => { observations++; await gate.promise; return observe(observation) }
  }
  if (setupFails) f.boundary.characteristic.startNotifications = async () => { throw new DOMException('private-enable', 'NetworkError') }
  try {
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 30 : delay, ...args)
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'late-supervisor-observation', maxBytes: 1048576, maxRecords: 20 } }
    )
    if (setupFails) expectConsoleError('[WebBluetoothGattRuntime.enableSubscription] Notification start rejected:', { errors: [{ name: 'NetworkError' }] })
    if (!setupFails) f.boundary.gatt.disconnect()
    await until(() => observations === 1 && f.owner.supervisor.status().cleanupFailure !== null, 'supervisor observation deadline')
    expect(f.owner.supervisor.status().authorityReleased).toBe(false)
    const pending = await f.controller.stopOrdered('late-supervisor-observation')
    expect(pending).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'reconciliation-unknown' } } } })
    const detail = JSON.parse(pending.recording.error.storage.detail)
    expect(JSON.stringify(detail)).toContain('WebRecordingOutcomeUnknown')
    if (setupFails) expect(detail.setupFailure).toMatchObject({ code: 'connection.lost' })
    gate.resolve()
    await until(() => f.owner.supervisor.status().authorityReleased, 'autonomous authority release')
    expect(observations).toBe(1)
    const eventual = await f.controller.stopOrdered('late-supervisor-observation')
    expect(eventual.hostCleanup.state).toBe('released')
    if (setupFails) expect(eventual.recording).toMatchObject({ state: 'failed', error: { kind: 'source' } })
    else expect(eventual.recording).toMatchObject({ state: 'interrupted', error: null })
  } finally {
    gate.resolve()
    global.setTimeout = originalTimer
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('late rejected terminal observation retains timeout and rejection evidence in the public receipt', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  let observations = 0
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'late-observation-rejection', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    journal.observeTerminal = () => { observations++; return gate.promise }
    journal.canRetryUnknown = () => false
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 30 : delay, ...args)
    f.boundary.gatt.disconnect()
    await until(() => observations === 1 && f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    gate.reject(new DOMException('private-observation', 'DataError'))
    await until(() => f.owner.supervisor.status().failure !== null, 'late rejection')
    const stop = await f.controller.stopOrdered('late-observation-rejection')
    expect(stop.recording.error).toMatchObject({ kind: 'storage', storage: { kind: 'reconciliation-unknown' } })
    const detail = stop.recording.error.storage.detail
    expect(detail.match(/WebRecordingOutcomeUnknown/g)?.length).toBeGreaterThanOrEqual(2)
    expect(detail).toContain('DataError')
    expect(detail).not.toContain('private-observation')
  } finally {
    gate.resolve()
    global.setTimeout = originalTimer
    f.owner.supervisor.journal.canRetryUnknown = () => true
    await f.manager.destroy()
  }
})

test('late terminal observation cannot release authority after authority loss', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  let observations = 0
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'late-observation-stale-authority', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = async observation => { observations++; await gate.promise; return observe(observation) }
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 30 : delay, ...args)
    f.boundary.gatt.disconnect()
    await until(() => observations === 1 && f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    f.owner.supervisor.loseAuthority()
    gate.resolve()
    await until(() => !f.owner.supervisor.status().storageOutcomeUnknown, 'late observation settlement')
    expect(f.owner.supervisor.status()).toMatchObject({ authorityReleased: false, state: 'failed' })
    expect((await f.controller.stopOrdered('late-observation-stale-authority')).hostCleanup.state).not.toBe('released')
  } finally {
    gate.resolve()
    global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})

test('public stop reports unknown finalization with its original setup evidence', async () => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const install = f.liveOwners.install.bind(f.liveOwners)
  let finalize
  const { webRecordingContractError } = require('../../src/web/web-recording-errors')
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    owner.install = () => { throw webRecordingContractError('connection.lost', 'connection', 'web-recording.install') }
    return owner
  }
  f.liveOwners.install = (identity, value) => {
    install(identity, value)
    finalize = value.supervisor.journal.finalizeUnstarted.bind(value.supervisor.journal)
    value.supervisor.journal.finalizeUnstarted = async () => { throw new Error('private-storage') }
    value.supervisor.journal.canRetryUnknown = () => false
  }
  try {
    const start = await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'unknown-unstarted-finalization', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => f.owner.supervisor.status().runtimeFailure !== null && f.owner.supervisor.status().sourceReleased, 'unknown finalization')
    const stop = await f.controller.stopOrdered('unknown-unstarted-finalization')
    expect(start.receipt.error).toMatchObject({ kind: 'source', source: { code: 'connection.lost', operation: 'web-recording.install' } })
    expect(stop).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'reconciliation-unknown' } } }, hostCleanup: { state: 'pending' } })
    const detail = JSON.parse(stop.recording.error.storage.detail)
    expect(detail.setupFailure).toMatchObject({ code: 'connection.lost', domain: 'connection', operation: 'web-recording.install' })
    expect(JSON.stringify(stop)).not.toContain('private-storage')
  } finally {
    f.owner.supervisor.journal.canRetryUnknown = () => true
    f.owner.supervisor.journal.finalizeUnstarted = finalize
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test.each(['before', 'after'])('public stop retries an installed membership removal that fails %s its effect', async timing => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const characteristic = f.boundary.characteristic
  const memberships = f.backend.gattRuntime.ingressMemberships
  let other, removes = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    const ingress = owner.ingress
    other = ingress.register(new Map([[characteristic, 'other']]), 4, 16)
    const changed = ingress.membershipChanged
    ingress.membershipChanged = (member, added, registration) => {
      if (!added && ++removes === 1) {
        if (timing === 'after') changed(member, added, registration)
        // An already-routed callback can reenter before the failed removal returns.
        ingress.admit(characteristic, new Uint8Array([4, 5]))
        throw new Error('private-detach')
      }
      changed(member, added, registration)
    }
    return owner
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'installed-detach-retry', maxBytes: 1048576, maxRecords: 20 } }
    )
    const first = await f.controller.stopOrdered('installed-detach-retry')
    expect(first.hostCleanup.state).not.toBe('released')
    expect([...memberships.get(characteristic).values()][0].size).toBe(timing === 'before' ? 2 : 1)
    // A callback already routed to this ingress can arrive during ambiguous removal.
    f.owner.supervisor.owner.ingress.admit(characteristic, new Uint8Array([7, 8, 9]))
    const retry = await f.controller.stopOrdered('installed-detach-retry')
    expect(retry.hostCleanup.state).toBe('released')
    expect(removes).toBe(2)
    const batch = await f.controller.prepareOrdered('installed-detach-retry', limits)
    expect(batch.records.map(row => row.record.kind)).toEqual(['value', 'failure-observation', 'failure-observation', 'terminal'])
    expect(batch.records.slice(1, 3).map(row => row.record.sourceRejection.bytes)).toEqual(['2', '3'])
    expect([...memberships.get(characteristic).values()][0].size).toBe(1)
    other.drain()
    f.owner.supervisor.owner.ingress.admit(characteristic, new Uint8Array([7]))
    expect(other.drain()).toMatchObject([{ kind: 'value', memberId: 'other', value: new Uint8Array([7]) }])
  } finally {
    other?.retire('closed')
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test.each(['trusted', 'untrusted'])('public diagnostic classifies the latest %s rollback attempt while retaining history', async kind => {
  const f = await publicFixture()
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const { webRecordingContractError } = require('../../src/web/web-recording-errors')
  const first = webRecordingContractError('connection.lost', 'connection', 'web-recording.first-cleanup')
  const second = kind === 'trusted'
    ? webRecordingContractError('platform.failure', 'cleanup', 'web-recording.second-cleanup')
    : new Error('private-second-cleanup')
  let removes = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    const ingress = owner.ingress
    const changed = ingress.membershipChanged
    ingress.membershipChanged = (member, added, registration) => {
      if (added) { changed(member, added, registration); throw new Error('private-install') }
      if (++removes === 1) throw first
      if (removes === 2) throw second
      changed(member, added, registration)
    }
    return owner
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'latest-rollback', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => removes === 2, 'second rollback failure')
    expect(f.owner.supervisor.status().cleanupFailure.errors).toEqual(expect.arrayContaining([first, second]))
    const diagnostic = await f.controller.runtimeDiagnostic('latest-rollback')
    expect(diagnostic).toMatchObject({ lastFailure: { error: { code: 'platform.failure', domain: 'cleanup', operation: kind === 'trusted' ? 'web-recording.second-cleanup' : 'web-recording.cleanup' } } })
    expect(JSON.stringify(diagnostic)).not.toContain('private-second-cleanup')
    expect((await f.controller.stopOrdered('latest-rollback')).hostCleanup.state).toBe('released')
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})


test('production observation timeout retains unknown storage until the transaction completes', async () => {
  const f = await publicFixture(); const originalTimer = global.setTimeout
  let entered = false, paused = false
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'production-observation-timeout', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => {
      if (!entered) { entered = true; paused = true; f.storage.pauseNextCompletion() }
      return observe(observation)
    }
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 5000 ? 25 : delay, ...args)
    f.owner.supervisor.stop()
    await until(() => entered, 'real observation dispatched')
    await f.storage.requestsProcessed()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'bridge timeout')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ storageOutcomeUnknown: true, authorityReleased: false })
    const stop = await f.owner.stopResult()
    expect(stop.recording.error).toMatchObject({ kind: 'storage', storage: { kind: 'reconciliation-unknown' } })
    f.storage.complete(); paused = false
    await until(() => f.owner.supervisor.status().authorityReleased, 'observation reconciled')
  } finally {
    if (paused) f.storage.complete()
    global.setTimeout = originalTimer
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('unstarted recording retries a late rejected terminal observation', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  const makeOwner = f.backend.createRecordingSourceOwner.bind(f.backend)
  const install = f.liveOwners.install.bind(f.liveOwners)
  let observations = 0
  f.backend.createRecordingSourceOwner = (...args) => {
    const owner = makeOwner(...args)
    owner.install = () => { throw new Error('installation failed') }
    return owner
  }
  f.liveOwners.install = (identity, owner) => {
    install(identity, owner)
    const journal = owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => ++observations === 1 ? gate.promise : observe(observation)
  }
  try {
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 25 : delay, ...args)
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'unstarted-late-rejection', maxBytes: 1048576, maxRecords: 20 } }
    )
    await until(() => observations === 1 && f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    gate.reject(new Error('observation rejected'))
    await until(() => f.owner.supervisor.status().failure instanceof AggregateError, 'late rejection')
    await f.owner.supervisor.retry()
    expect(f.owner.supervisor.status()).toMatchObject({ authorityReleased: true, storageOutcomeUnknown: false })
    expect(observations).toBe(2)
  } finally {
    gate.resolve(); global.setTimeout = originalTimer
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('authority loss retains late observation rejection and unresolved storage evidence', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'stale-observation-rejection', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    journal.observeTerminal = () => gate.promise
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 25 : delay, ...args)
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    f.owner.supervisor.loseAuthority()
    const authorityFailure = f.owner.supervisor.status().runtimeFailure.error
    gate.reject(new DOMException('private-rejection', 'DataError'))
    await until(() => f.owner.supervisor.status().runtimeFailure?.error !== authorityFailure, 'late rejection retained')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ storageOutcomeUnknown: true, authorityReleased: false })
    const stop = await f.controller.stopOrdered('stale-observation-rejection')
    expect(stop.recording.error.storage).toMatchObject({ kind: 'reconciliation-unknown' })
    expect(stop.recording.error.storage.detail).toContain('DataError')
    expect(stop.recording.error.storage.detail).toContain('WebRecordingAuthorityLost')
  } finally {
    gate.resolve(); global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})


test('production bridge prompt observation rejection reports proven noncommit until retry succeeds', async () => {
  const f = await publicFixture()
  const transaction = f.storage.transaction.bind(f.storage)
  let fail = false, attempts = 0
  f.storage.transaction = (...args) => {
    if (fail) { fail = false; throw new DOMException('private-transaction', 'DataError') }
    return transaction(...args)
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'prompt-observation-rejection', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => { if (++attempts === 1) fail = true; return observe(observation) }
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'prompt rejection')
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    const stop = await f.owner.stopResult()
    expect(stop.recording.error).toMatchObject({ kind: 'storage', storage: { kind: 'indexeddb-transaction' } })
    expect(stop.recording.error.storage.detail).toContain('DataError')
    expect(stop.recording.error.storage.detail).not.toContain('private-transaction')
    await f.owner.supervisor.retry()
    expect(f.owner.supervisor.status()).toMatchObject({ authorityReleased: true, storageOutcomeUnknown: false })
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('a proven late observation failure clears uncertainty without discarding stale authority failure', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  const rejection = new DOMException('private-abort', 'AbortError')
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'stale-observation-proven-abort', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    journal.observeTerminal = () => gate.promise
    journal.isDefinitiveFailure = error => error === rejection
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 25 : delay, ...args)
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    f.owner.supervisor.loseAuthority()
    const authorityFailure = f.owner.supervisor.status().runtimeFailure.error
    gate.reject(rejection)
    await until(() => f.owner.supervisor.status().runtimeFailure?.error !== authorityFailure, 'proven rejection')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    const flatten = error => error instanceof AggregateError ? error.errors.flatMap(flatten) : [error]
    const failures = flatten(status.failure)
    expect(failures).toContain(rejection)
    expect(failures.some(error => error.name === 'WebRecordingAuthorityLost')).toBe(true)
  } finally {
    gate.resolve(); global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})


test('observation success after authority loss before deadline resolves storage without releasing authority', async () => {
  const f = await publicFixture(); const gate = deferred()
  let started = false
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'stale-prompt-observation-success', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    journal.observeTerminal = () => { started = true; return gate.promise }
    f.owner.supervisor.stop()
    await until(() => started, 'observation started')
    f.owner.supervisor.loseAuthority()
    gate.resolve()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'observation settled')
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false, state: 'failed' })
  } finally {
    gate.resolve()
    await f.manager.destroy()
  }
})


test('a late unknown-outcome rejection retains authority loss and the timeout', async () => {
  const f = await publicFixture(); const gate = deferred(); const originalTimer = global.setTimeout
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'stale-observation-unknown-rejection', maxBytes: 1048576, maxRecords: 20 } }
    )
    f.owner.supervisor.journal.observeTerminal = () => gate.promise
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 30000 ? 25 : delay, ...args)
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'observation deadline')
    const timeout = f.owner.supervisor.status().cleanupFailure
    f.owner.supervisor.loseAuthority()
    const authorityFailure = f.owner.supervisor.status().runtimeFailure.error
    gate.reject(timeout)
    await until(() => f.owner.supervisor.status().runtimeFailure?.error !== authorityFailure, 'late rejection')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ storageOutcomeUnknown: true, authorityReleased: false })
    const flatten = error => error instanceof AggregateError ? error.errors.flatMap(flatten) : [error]
    expect(flatten(status.failure)).toEqual(expect.arrayContaining([timeout, expect.objectContaining({ name: 'WebRecordingAuthorityLost' })]))
  } finally {
    gate.resolve(); global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})


test('authority loss after a prompt production observation rejection retains both failures', async () => {
  const f = await publicFixture(); const transaction = f.storage.transaction.bind(f.storage)
  let fail = false
  f.storage.transaction = (...args) => {
    if (fail) { fail = false; throw new DOMException('private-rejection', 'DataError') }
    return transaction(...args)
  }
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'prompt-rejection-then-authority-loss', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => { fail = true; return observe(observation) }
    f.owner.supervisor.stop()
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'prompt rejection')
    f.owner.supervisor.loseAuthority()
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    const stop = await f.owner.stopResult()
    expect(stop.recording.error.storage.kind).toBe('indexeddb-transaction')
    expect(stop.recording.error.storage.detail).toContain('DataError')
    expect(stop.recording.error.storage.detail).toContain('WebRecordingAuthorityLost')
  } finally { await f.manager.destroy() }
})

test.each(['complete', 'abort'])('production observation %s after timeout and authority loss reaches public stop', async settlement => {
  const f = await publicFixture(); const originalTimer = global.setTimeout
  let entered = false, paused = false
  const id = `actual-late-${settlement}`
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id, maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => {
      if (!entered) { entered = true; paused = true; f.storage.pauseNextCompletion() }
      return observe(observation)
    }
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 5000 ? 25 : delay, ...args)
    const stop = f.controller.stopOrdered(id)
    await until(() => entered, 'production observation entered')
    await f.storage.requestsProcessed()
    const first = await stop
    expect(first).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'reconciliation-unknown' } } }, hostCleanup: { state: 'release-failed' } })
    f.coordinator.lose()
    await until(() => f.owner.supervisor.status().runtimeFailure?.error !== null && f.owner.supervisor.status().failure?.name === 'AggregateError', 'coordinator authority loss')
    if (settlement === 'abort') f.storage.abortNext(new DOMException('private-abort', 'AbortError'))
    f.storage.complete(); paused = false
    await until(() => !f.owner.supervisor.status().storageOutcomeUnknown, 'late transaction outcome')
    const status = f.owner.supervisor.status()
    expect(status).toMatchObject({ authorityReleased: false, storageOutcomeUnknown: false })
    const later = await f.controller.stopOrdered(id)
    expect(later).toMatchObject({ recording: { state: 'failed' }, hostCleanup: { state: 'pending' } })
    expect(later.recording.error.storage?.kind).not.toBe('reconciliation-unknown')
    if (settlement === 'abort') expect(JSON.stringify(later)).toContain('AbortError')
    expect(JSON.stringify(later)).not.toContain('private-abort')
  } finally {
    if (paused) f.storage.complete()
    global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})

test('production observation transaction creation failure is proven absent through public stop and retry', async () => {
  const f = await publicFixture(); const transaction = f.storage.transaction.bind(f.storage)
  let fail = false
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'actual-not-started', maxBytes: 1048576, maxRecords: 20 } }
    )
    f.storage.transaction = (...args) => {
      if (fail) { fail = false; throw new DOMException('private-create', 'DataError') }
      return transaction(...args)
    }
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => { fail = true; journal.observeTerminal = observe; return observe(observation) }
    const first = await f.controller.stopOrdered('actual-not-started')
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    expect(first).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage' } }, hostCleanup: { state: 'release-failed' } })
    expect(first.recording.error.storage.kind).not.toBe('reconciliation-unknown')
    expect(JSON.stringify(first)).toContain('DataError')
    expect(JSON.stringify(first)).not.toContain('private-create')
    const retry = await f.controller.stopOrdered('actual-not-started')
    expect(retry).toMatchObject({ recording: { state: 'finished', error: null }, hostCleanup: { state: 'released' } })
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})


test('production observation commits before timeout after actual coordinator loss', async () => {
  const f = await publicFixture()
  let entered = false, paused = false
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'actual-stale-prompt', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => {
      if (!entered) { entered = true; paused = true; f.storage.pauseNextCompletion() }
      return observe(observation)
    }
    const stop = f.controller.stopOrdered('actual-stale-prompt')
    await until(() => entered, 'observation dispatched')
    await f.storage.requestsProcessed()
    f.coordinator.lose()
    await until(() => f.owner.supervisor.status().failure?.name === 'WebRecordingAuthorityLost', 'coordinator loss')
    f.storage.complete(); paused = false
    const result = await stop
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    expect(result).toMatchObject({ recording: { state: 'failed' }, hostCleanup: { state: 'release-failed' } })
    expect(result.recording.error.storage?.kind).not.toBe('reconciliation-unknown')
    expect(f.storage.rows('recordings')[0].terminalObservation).toBeDefined()
  } finally {
    if (paused) f.storage.complete()
    await f.manager.destroy()
  }
})

test('confirmed production observation abort reports storage failure and public retry finishes', async () => {
  const f = await publicFixture()
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'actual-prompt-abort', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = observation => {
      f.storage.abortNext(new DOMException('private-abort', 'AbortError'))
      journal.observeTerminal = observe
      return observe(observation)
    }
    const result = await f.controller.stopOrdered('actual-prompt-abort')
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    expect(result).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'indexeddb-transaction' } } }, hostCleanup: { state: 'release-failed' } })
    expect(result.recording.error.storage.detail).toContain('AbortError')
    expect(result.recording.error.storage.detail).not.toContain('private-abort')
    expect(await f.controller.stopOrdered('actual-prompt-abort')).toMatchObject({ recording: { state: 'finished', error: null }, hostCleanup: { state: 'released' } })
  } finally {
    await f.owner?.supervisor.retry().catch(() => {})
    await f.manager.destroy()
  }
})

test('an abort request is unknown until the production transaction confirms abort', async () => {
  const f = await publicFixture(); const originalTimer = global.setTimeout
  const transaction = f.storage.transaction.bind(f.storage)
  let observation = false, tx, paused = false
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'actual-abort-request', maxBytes: 1048576, maxRecords: 20 } }
    )
    f.storage.transaction = (...args) => {
      const result = transaction(...args)
      if (observation) { observation = false; tx = result }
      return result
    }
    const journal = f.owner.supervisor.journal
    const observe = journal.observeTerminal.bind(journal)
    journal.observeTerminal = value => {
      observation = true; paused = true; f.storage.pauseNextCompletion()
      return observe(value)
    }
    global.setTimeout = (callback, delay, ...args) => originalTimer(callback, delay === 5000 ? 25 : delay, ...args)
    const stop = f.controller.stopOrdered('actual-abort-request')
    await until(() => tx !== undefined, 'observation transaction')
    await f.storage.requestsProcessed()
    tx.abort()
    const result = await stop
    expect(result).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'reconciliation-unknown' } } }, hostCleanup: { state: 'release-failed' } })
    expect(f.owner.supervisor.status().storageOutcomeUnknown).toBe(true)
    f.coordinator.lose()
    await until(() => f.owner.supervisor.status().failure?.name === 'AggregateError', 'coordinator loss')
    f.storage.complete(); paused = false
    await until(() => !f.owner.supervisor.status().storageOutcomeUnknown, 'confirmed abort')
    const after = await f.controller.stopOrdered('actual-abort-request')
    expect(after).toMatchObject({ recording: { state: 'failed', error: { kind: 'storage', storage: { kind: 'indexeddb-transaction' } } }, hostCleanup: { state: 'pending' } })
    expect(f.owner.supervisor.status().authorityReleased).toBe(false)
  } finally {
    if (paused) f.storage.complete()
    global.setTimeout = originalTimer
    await f.manager.destroy()
  }
})

test('late observation rejection remains visible after exact reconciliation and authority loss', async () => {
  const f = await publicFixture(); const observation = deferred(); const release = deferred(); const timer = global.setTimeout
  let reconciling = false, releasing = false
  const rejection = new DOMException('private-late', 'DataError')
  try {
    await f.database.reserveOrderedRecordingStart().start(
      [{ memberId: 'heart', characteristic: f.characteristic }],
      { recording: { id: 'reconciled-late-failure', maxBytes: 1048576, maxRecords: 20 } }
    )
    const journal = f.owner.supervisor.journal
    journal.observeTerminal = () => observation.promise
    journal.reconcileObservation = async () => { reconciling = true; return 'committed' }
    journal.releaseAuthority = () => { releasing = true; return release.promise }
    global.setTimeout = (callback, delay, ...args) => timer(callback, delay === 30000 ? 25 : delay, ...args)
    f.owner.supervisor.stop()
    await until(() => reconciling && releasing, 'observation reconciled')
    f.owner.supervisor.loseAuthority()
    release.reject(new Error('release failed'))
    await until(() => f.owner.supervisor.status().cleanupFailure !== null, 'release failure')
    observation.reject(rejection)
    const flatten = error => error instanceof AggregateError ? error.errors.flatMap(flatten) : [error]
    await until(() => flatten(f.owner.supervisor.status().failure).includes(rejection), 'late failure retained')
    expect(f.owner.supervisor.status()).toMatchObject({ storageOutcomeUnknown: false, authorityReleased: false })
    const result = await f.controller.stopOrdered('reconciled-late-failure')
    expect(result).toMatchObject({ recording: { state: 'failed' }, hostCleanup: { state: 'pending' } })
  } finally {
    observation.resolve(); release.resolve(); global.setTimeout = timer
    await f.manager.destroy()
  }
})
