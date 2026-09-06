// __tests__/core/unified-lifecycle-hardening.test.js

const {
  attachBleBackend,
  BleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const {
  capacity,
  deadline,
  monotonicTimestamp,
  opaqueId,
  ownBytes,
  version,
  versionRange
} = require('../../src/backend-contract/primitives')
const {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration
} = require('../../src/backend-contract/capabilities')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { awaitSignal } = require('../helpers/async')

const maximumBytes = 512 * 1024

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function delivery() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(1024),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
}

function operation(signal = null, operationDeadline = null) {
  return { signal, deadline: operationDeadline }
}

function subscriptionOptions(signal = null) {
  return { ...operation(signal), delivery: delivery() }
}

function scanOptions() {
  return {
    filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: false }
  }
}

function advertisement(rawRecord) {
  const absent = { state: 'absent', reason: 'test-not-observed', provenance: 'not-provided' }
  return {
    device: {
      id: peer(),
      backendInstanceId: opaqueId('deterministic-backend', 'backend-instance', 'deterministic'),
      scope: 'backend',
      stableAcrossRestarts: false,
      address: null
    },
    provenance: 'platform-raw',
    sourceTimestamp: absent,
    receivedAtMonotonicMs: monotonicTimestamp(1),
    ingressOrdinal: 1,
    scanSessionId: opaqueId('deterministic-scan', 'scan-session', 'deterministic'),
    localName: absent,
    rssi: absent,
    txPower: absent,
    connectable: absent,
    appearance: absent,
    serviceUuids: absent,
    solicitedServiceUuids: absent,
    overflowServiceUuids: absent,
    serviceData: absent,
    manufacturerData: absent,
    rawRecord: { state: 'present', value: ownBytes(rawRecord, maximumBytes), provenance: 'observed' },
    scanResponseRecord: absent
  }
}

function peer() {
  return opaqueId('deterministic-peer', 'peer', 'deterministic')
}

function managerConstruction(attachedBackend) {
  return {
    attachedBackend,
    clientId: opaqueId('lifecycle-client', 'client', 'deterministic:lifecycle-client'),
    managerId: opaqueId('lifecycle-manager', 'manager', 'deterministic:lifecycle-manager'),
    ownerMode: 'owning'
  }
}

async function createFixture(backendOptions = {}) {
  const fixture = createDeterministicTestBackend(backendOptions)
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const manager = await BleManager.create(managerConstruction(attachedBackend), authority, DEFAULT_BLE_MANAGER_OPTIONS)
  return { fixture, manager }
}

async function createManagerFixture(ownerMode) {
  const fixture = createDeterministicTestBackend()
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const owner = await BleManager.create(managerConstruction(attachedBackend), authority, DEFAULT_BLE_MANAGER_OPTIONS)
  if (ownerMode === 'owning') {
    return { fixture, manager: owner, owner: null }
  }
  const borrower = await BleManager.create(
    {
      ...managerConstruction(attachedBackend),
      clientId: opaqueId('lifecycle-borrower-client', 'client', 'deterministic:lifecycle-borrower-client'),
      managerId: opaqueId('lifecycle-borrower-manager', 'manager', 'deterministic:lifecycle-borrower-manager'),
      ownerMode: 'borrowing'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  return { fixture, manager: borrower, owner }
}

async function settle(controller, promise) {
  let settled = false
  void promise.then(
    () => {
      settled = true
    },
    () => {
      settled = true
    }
  )
  for (let attempt = 0; attempt < 100 && !settled; attempt += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
  return promise
}

async function flushMicrotasks() {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

async function flushVirtual(controller) {
  for (let turn = 0; turn < 32; turn += 1) {
    controller.clock.runUntilIdle()
    await Promise.resolve()
  }
}

/**
 * How a promise settled, without racing a timer to find out.
 *
 * The earlier form resolved to `{ state: 'pending' }` when a 250ms sleep won,
 * which made a loaded runner report the cancellation as never having happened.
 * Reflecting the promise and awaiting it says the same thing about the code and
 * nothing about the machine.
 */
async function settleOutcome(promise, description) {
  return awaitSignal(
    promise.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    ),
    description
  )
}

async function connectedDatabase(fixture, manager) {
  const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
  const database = await settle(fixture.controller, connection.discover(operation()))
  const snapshot = await database.snapshot()
  return { connection, database, characteristic: snapshot.characteristics[0].path }
}

function notificationAddress(path) {
  return {
    serviceUuid: path.serviceUuid,
    serviceOccurrence: Number(path.serviceOccurrence),
    characteristicUuid: path.characteristicUuid,
    characteristicOccurrence: Number(path.characteristicOccurrence)
  }
}

function expectNoResources(counters) {
  expect(Object.entries(counters).filter(([, value]) => Number(value) !== 0)).toEqual([])
}

describe('UnifiedBleCore lifecycle hardening', () => {
  test('rejects stale, mismatched, and impossible public lifecycle transitions before finishing a connection', async () => {
    const { fixture, manager } = await createFixture()
    const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
    const coreConnection = connection.connection

    coreConnection.applyBackendTransition('connected', 'disconnecting', 20)
    expect(() => coreConnection.applyBackendTransition('disconnecting', 'disconnecting', 20)).toThrow(
      'connection-lifecycle.ingress-order'
    )
    expect(() => coreConnection.applyBackendTransition('connected', 'disconnecting', 21)).toThrow(
      'connection-lifecycle.previous-state'
    )
    expect(() => coreConnection.applyBackendTransition('disconnecting', 'connected', 21)).toThrow(
      'connection-lifecycle.transition'
    )

    coreConnection.finishBackendLifecycle('disconnecting', 'disconnected', 'requested-disconnect', 21)
    await settle(fixture.controller, connection.release())
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('gives a coalesced discovery joiner its own abort terminal without cancelling the first caller', async () => {
    const { fixture, manager } = await createFixture()
    const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
    fixture.controller.queueCompletion('discover', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const first = connection.discover(operation())
    await flushMicrotasks()
    const abortController = new AbortController()
    const joiner = connection.discover(operation(abortController.signal))
    await flushMicrotasks()

    abortController.abort()

    await expect(joiner).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(settle(fixture.controller, first)).resolves.toBeDefined()
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('keeps coalesced subscription enablement alive when a joiner aborts and counts CCCD only once ready', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    fixture.controller.queueCompletion('subscribe', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const first = database.subscribe(characteristic, subscriptionOptions())
    await flushMicrotasks()
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)

    const abortController = new AbortController()
    const joiner = database.subscribe(characteristic, subscriptionOptions(abortController.signal))
    await flushMicrotasks()
    abortController.abort()

    await expect(joiner).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(Number(fixture.backend.resourceCounters().subscriptionConsumers)).toBe(1)
    const subscription = await settle(fixture.controller, first)
    expect(subscription.values).toBeDefined()
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    await settle(fixture.controller, subscription.remove())
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('compensates a late non-cancellable subscribe before dispatching the next queued operation', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    fixture.controller.queueCompletion('subscribe', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const abortController = new AbortController()
    const subscribe = database.subscribe(characteristic, subscriptionOptions(abortController.signal))
    await flushMicrotasks()
    abortController.abort()

    await expect(subscribe).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const laterRead = database.read(characteristic, operation())
    await flushMicrotasks()
    expect(fixture.controller.clock.pendingTaskCount()).toBe(1)

    await flushVirtual(fixture.controller)
    await expect(laterRead).resolves.toBeInstanceOf(Uint8Array)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(Number(fixture.backend.resourceCounters().subscriptionConsumers)).toBe(0)
    expect(manager.traces().some(record => record.transition === 'late-subscription-compensated')).toBe(true)
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('records a failed late-subscription compensation and retries a failed explicit removal', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    fixture.controller.queueCompletion('subscribe', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const abortController = new AbortController()
    const cancelled = database.subscribe(characteristic, subscriptionOptions(abortController.signal))
    await flushMicrotasks()
    abortController.abort()
    await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await flushVirtual(fixture.controller)
    expect(manager.traces().some(record => record.transition === 'late-subscription-compensation-failed')).toBe(true)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)

    const active = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    await expect(settle(fixture.controller, active.remove())).resolves.toMatchObject({ state: 'release-failed' })
    await expect(settle(fixture.controller, active.remove())).resolves.toEqual({ state: 'released', failures: [] })
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('retries the physical disable when the same subscription is removed again after failure', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })

    await expect(settle(fixture.controller, subscription.remove())).resolves.toMatchObject({
      state: 'release-failed'
    })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)

    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    await expect(settle(fixture.controller, subscription.remove())).resolves.toEqual({
      state: 'released',
      failures: []
    })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('settles owner destruction with an active subscription and its cached backend event stream', async () => {
    const { fixture, manager } = await createFixture()
    const scan = await settle(fixture.controller, manager.scan(scanOptions()))
    const observations = scan.observations[Symbol.asyncIterator]()
    const observation = observations.next()
    fixture.controller.emitAdvertisement(advertisement(new Uint8Array([1])))
    await expect(observation).resolves.toMatchObject({ done: false, value: { kind: 'value' } })
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    const values = subscription.values[Symbol.asyncIterator]()
    const value = values.next()
    fixture.controller.emitNotification(notificationAddress(characteristic), new Uint8Array([2]))
    await expect(value).resolves.toMatchObject({ done: false, value: { kind: 'value' } })
    await expect(settle(fixture.controller, manager.destroy())).resolves.toEqual({ state: 'released', failures: [] })
    expect(manager.state).toBe('destroyed')
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('accounts only queued payload bytes and rejects a path with a mismatched attachment generation', async () => {
    const { fixture, manager } = await createFixture()
    const scan = await settle(fixture.controller, manager.scan(scanOptions()))
    fixture.controller.emitAdvertisement(advertisement(new Uint8Array([1, 2, 3])))
    await flushMicrotasks()
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(3)
    await expect(scan.observations[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'value' }
    })
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(0)

    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    fixture.controller.emitNotification(notificationAddress(characteristic), new Uint8Array([7, 8]))
    await flushMicrotasks()
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(2)
    await expect(subscription.values[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'value' }
    })
    expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(0)

    const staleAttachmentPath = {
      ...characteristic,
      attachment: { ...characteristic.attachment, backendGeneration: 'mismatched-generation' }
    }
    await expect(database.read(staleAttachmentPath, operation())).rejects.toMatchObject({
      normalized: { code: 'gatt.stale-handle' }
    })
    await settle(fixture.controller, subscription.remove())
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('retains failed database-child cleanup until connection release retries successfully', async () => {
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })

    await expect(settle(fixture.controller, connection.release())).resolves.toMatchObject({
      state: 'release-failed'
    })
    expect(Number(manager.localResourceCounters().databaseSnapshots)).toBe(0)
    expect(Number(manager.localResourceCounters().connectionLeases)).toBe(1)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)

    await expect(settle(fixture.controller, connection.release())).resolves.toEqual({
      state: 'released',
      failures: []
    })
    expect(Number(manager.localResourceCounters().databaseSnapshots)).toBe(0)
    expect(Number(manager.localResourceCounters().connectionLeases)).toBe(0)
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('connection cleanup receipt retains readiness-source cleanup failure', async () => {
    const readinessRegistration = createBackendOperationCapabilityRegistration({
      id: BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness,
      implementationVersion: 'test',
      sourceDigest: 'test-readiness-v1',
      tckSuiteId: 'test.connection-cleanup',
      requiredScenarioIds: ['test.connection-cleanup']
    })
    const { fixture, manager } = await createFixture({ featureRegistrations: [readinessRegistration] })
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    const readinessEvents = new CoreBoundedStream(
      { itemCapacity: capacity(4), byteCapacity: capacity(1024), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const cleanupFailure = {
      resourceKind: 'gatt.write-readiness',
      error: {
        code: 'platform.failure',
        domain: 'cleanup',
        operation: 'test.connection-late-readiness-close',
        platform: null,
        retryability: 'never'
      }
    }
    const readinessClose = jest.fn(async () => ({ state: 'release-failed', failures: [cleanupFailure] }))
    fixture.backend.connections = {
      ...fixture.backend.connections,
      writeWithoutResponseReadiness: async () => ({ events: readinessEvents, close: readinessClose })
    }
    const abortController = new AbortController()
    const pendingWrite = database.writeWhenReady(characteristic, new Uint8Array([1]), {
      ...operation(abortController.signal),
      mode: 'without-response'
    })

    await flushMicrotasks()
    abortController.abort()
    await expect(pendingWrite).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })

    await expect(settle(fixture.controller, connection.release())).resolves.toEqual({
      state: 'release-failed',
      failures: [cleanupFailure]
    })
    expect(readinessClose).toHaveBeenCalledTimes(1)
    await expect(settle(fixture.controller, connection.release())).resolves.toEqual({ state: 'released', failures: [] })
    await expect(settle(fixture.controller, manager.destroy())).resolves.toEqual({ state: 'released', failures: [] })
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('continues backend teardown and merges admission and child cleanup failures', async () => {
    const readinessRegistration = createBackendOperationCapabilityRegistration({
      id: BUILT_IN_FEATURE_IDS.writeWithoutResponseReadiness,
      implementationVersion: 'test',
      sourceDigest: 'test-readiness-v1',
      tckSuiteId: 'test.connection-cleanup',
      requiredScenarioIds: ['test.connection-cleanup']
    })
    const { fixture, manager } = await createFixture({ featureRegistrations: [readinessRegistration] })
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    const readinessEvents = new CoreBoundedStream(
      { itemCapacity: capacity(4), byteCapacity: capacity(1024), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const admissionFailure = {
      resourceKind: 'gatt.write-readiness',
      error: {
        code: 'platform.failure',
        domain: 'cleanup',
        operation: 'test.connection-admission-close',
        platform: null,
        retryability: 'never'
      }
    }
    const childFailure = {
      resourceKind: 'gatt.database-child',
      error: {
        code: 'platform.failure',
        domain: 'cleanup',
        operation: 'test.connection-child-cleanup',
        platform: null,
        retryability: 'never'
      }
    }
    const readinessClose = jest.fn(async () => ({ state: 'release-failed', failures: [admissionFailure] }))
    fixture.backend.connections = {
      ...fixture.backend.connections,
      writeWithoutResponseReadiness: async () => ({ events: readinessEvents, close: readinessClose })
    }
    const cleanupChildren = jest
      .spyOn(connection.connection, 'cleanupChildren')
      .mockResolvedValueOnce({ state: 'release-failed', failures: [childFailure] })
      .mockResolvedValue({ state: 'released', failures: [] })
    const abortController = new AbortController()
    const pendingWrite = database.writeWhenReady(characteristic, new Uint8Array([1]), {
      ...operation(abortController.signal),
      mode: 'without-response'
    })

    await flushMicrotasks()
    abortController.abort()
    await expect(pendingWrite).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })

    await expect(settle(fixture.controller, connection.release())).resolves.toEqual({
      state: 'release-failed',
      failures: [admissionFailure, childFailure]
    })
    expect(cleanupChildren).toHaveBeenCalledWith('owner-released')
    expect(readinessClose).toHaveBeenCalledTimes(1)
    expect(Number(fixture.backend.resourceCounters().physicalLinks)).toBe(0)

    await expect(settle(fixture.controller, connection.release())).resolves.toEqual({
      state: 'released',
      failures: []
    })
    await expect(settle(fixture.controller, manager.destroy())).resolves.toEqual({ state: 'released', failures: [] })
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('retries failed database-child cleanup before rediscovery and decrements the old snapshot once', async () => {
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    fixture.controller.queueCompletion('unsubscribe', {
      delayMs: 1,
      failure: 'platform.failure',
      cancellable: false,
      deadlineOrder: 'completion-first'
    })

    await expect(settle(fixture.controller, connection.discover(operation()))).rejects.toMatchObject({
      normalized: { code: 'platform.failure' }
    })
    expect(Number(manager.localResourceCounters().databaseSnapshots)).toBe(0)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)

    const rediscovered = await settle(fixture.controller, connection.discover(operation()))
    expect(rediscovered).toBeDefined()
    expect(Number(manager.localResourceCounters().databaseSnapshots)).toBe(1)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('settles pending and queued writes exactly once when Services Changed invalidates their database', async () => {
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    fixture.controller.queueCompletion('write', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const originalWrite = fixture.backend.gatt.write
    let writeDispatches = 0
    fixture.backend.gatt.write = (path, request) => {
      writeDispatches += 1
      return originalWrite(path, request)
    }

    const first = database.writeLong(characteristic, new Uint8Array(17), {
      ...operation(),
      mode: 'with-response'
    })
    await flushMicrotasks()
    fixture.controller.clock.advanceBy(0)
    await flushMicrotasks()
    expect(writeDispatches).toBe(1)

    const second = database.write(characteristic, new Uint8Array([2]), {
      ...operation(),
      mode: 'with-response'
    })
    expect(Number(manager.localResourceCounters().queuedOperations)).toBe(1)

    let firstSettles = 0
    let secondSettles = 0
    let firstReceipt = null
    let firstError = null
    let secondError = null
    void first.then(
      value => {
        firstSettles += 1
        firstReceipt = value
      },
      error => {
        firstSettles += 1
        firstError = error
      }
    )
    void second.then(
      () => {
        secondSettles += 1
      },
      error => {
        secondSettles += 1
        secondError = error
      }
    )

    try {
      fixture.controller.triggerServicesChanged(connection.peerId)
      await flushMicrotasks()

      expect(firstSettles).toBe(1)
      expect(secondSettles).toBe(1)
      expect(writeDispatches).toBe(1)
      expect(firstReceipt).toMatchObject({
        terminal: { outcome: 'disconnected', cause: 'operation.disconnected' },
        commitState: 'unknown',
        planState: 'planned',
        chunks: [{ state: 'uncertain' }]
      })
      expect(firstError).toBeNull()
      expect(secondError).toMatchObject({ normalized: { code: 'operation.disconnected' } })
    } finally {
      fixture.controller.clock.advanceBy(10)
      await flushMicrotasks()
      fixture.backend.gatt.write = originalWrite
      await settle(fixture.controller, manager.destroy())
    }
  })

  test('quarantines pending and queued writes before reasoned rediscovery without replaying them', async () => {
    const { fixture, manager } = await createFixture()
    let originalWrite = null
    try {
      const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
      const changedIterator = database.changed[Symbol.asyncIterator]()
      fixture.controller.queueCompletion('write', {
        delayMs: 10,
        failure: null,
        cancellable: false,
        deadlineOrder: 'completion-first'
      })
      originalWrite = fixture.backend.gatt.write
      let writeDispatches = 0
      fixture.backend.gatt.write = (path, request) => {
        writeDispatches += 1
        return originalWrite(path, request)
      }

      const first = database.writeLong(characteristic, new Uint8Array(17), {
        ...operation(),
        mode: 'with-response'
      })
      await flushMicrotasks()
      fixture.controller.clock.advanceBy(0)
      await flushMicrotasks()
      expect(writeDispatches).toBe(1)

      const second = database.write(characteristic, new Uint8Array([2]), {
        ...operation(),
        mode: 'with-response'
      })
      expect(Number(manager.localResourceCounters().queuedOperations)).toBe(1)

      const rediscovery = connection.rediscoverGatt(operation(), 'manual-rediscovery')
      await flushMicrotasks()

      await expect(first).resolves.toMatchObject({
        terminal: { outcome: 'disconnected', cause: 'operation.disconnected' },
        commitState: 'unknown',
        planState: 'planned',
        chunks: [{ state: 'uncertain' }]
      })
      await expect(second).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
      await expect(changedIterator.next()).resolves.toMatchObject({
        value: {
          kind: 'value',
          value: {
            reason: 'manual-rediscovery',
            affectedHandleRange: null
          }
        }
      })
      expect(writeDispatches).toBe(1)

      fixture.controller.clock.advanceBy(10)
      const replacement = await settle(fixture.controller, rediscovery)
      expect(replacement).toBeDefined()
      expect(writeDispatches).toBe(1)
    } finally {
      fixture.controller.clock.advanceBy(10)
      await flushMicrotasks()
      if (originalWrite !== null) fixture.backend.gatt.write = originalWrite
      await settle(fixture.controller, manager.destroy())
    }
  })

  test.each([['abort'], ['deadline']])('bounds reasoned rediscovery quarantine recovery after %s', async _kind => {
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    const originalDiscover = fixture.backend.gatt.discover
    const originalWrite = fixture.backend.gatt.write
    let discoverDispatches = 0
    let resolveWrite = null
    fixture.backend.gatt.discover = async (...args) => {
      discoverDispatches += 1
      return originalDiscover(...args)
    }
    fixture.backend.gatt.write = (_path, request) => ({
      completion: new Promise(resolve => {
        resolveWrite = () =>
          resolve({
            terminal: {
              correlation: request.operation.correlation,
              outcome: 'succeeded',
              cause: null
            },
            commitState: 'confirmed'
          })
      }),
      requestCancellation: async () => undefined
    })
    const first = database.write(characteristic, new Uint8Array([1]), {
      ...operation(),
      mode: 'with-response'
    })
    const firstOutcome = first.then(
      () => null,
      error => error
    )
    await flushMicrotasks()
    expect(Number(manager.localResourceCounters().dispatchedOperations)).toBe(1)

    const abortController = new AbortController()
    const cancellation =
      _kind === 'abort'
        ? { options: operation(abortController.signal), cancel: () => abortController.abort() }
        : { options: operation(null, deadline(database.monotonicNow() + 50)), cancel: () => undefined }
    const rediscovery = connection.rediscoverGatt(cancellation.options, 'manual-rediscovery')
    await flushMicrotasks()
    cancellation.cancel()

    try {
      const outcome = await settleOutcome(rediscovery, 'the cancelled rediscovery to settle')
      expect(outcome.state).toBe('rejected')
      expect(outcome.error).toMatchObject({
        normalized: { code: _kind === 'abort' ? 'operation.aborted' : 'operation.timed-out' }
      })
      expect(discoverDispatches).toBe(0)

      resolveWrite()
      await flushVirtual(fixture.controller)
      await expect(firstOutcome).resolves.toMatchObject({ normalized: { code: 'operation.disconnected' } })
    } finally {
      fixture.backend.gatt.discover = originalDiscover
      fixture.backend.gatt.write = originalWrite
      if (resolveWrite !== null) resolveWrite()
      await flushVirtual(fixture.controller)
      await settle(fixture.controller, manager.destroy())
    }
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('rejects malformed resolved read and write terminals instead of accepting backend values', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const originalRead = fixture.backend.gatt.read
    fixture.backend.gatt.read = (path, request) => {
      const dispatch = originalRead(path, request)
      return {
        ...dispatch,
        completion: dispatch.completion.then(result => ({
          ...result,
          terminal: { ...result.terminal, outcome: 'failed', cause: 'platform.failure' }
        }))
      }
    }
    await expect(settle(fixture.controller, database.read(characteristic, operation()))).rejects.toMatchObject({
      normalized: { code: 'protocol.violation' }
    })
    fixture.backend.gatt.read = originalRead

    const originalWrite = fixture.backend.gatt.write
    fixture.backend.gatt.write = (path, request) => {
      const dispatch = originalWrite(path, request)
      return {
        ...dispatch,
        completion: dispatch.completion.then(result => ({
          ...result,
          terminal: { ...result.terminal, cause: 'platform.failure' }
        }))
      }
    }
    await expect(
      settle(
        fixture.controller,
        database.write(characteristic, new Uint8Array([1, 2, 3]), {
          ...operation(),
          mode: 'with-response'
        })
      )
    ).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
    fixture.backend.gatt.write = originalWrite

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('compensates malformed resolved subscribe terminals, including late acknowledgements', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const originalSubscribe = fixture.backend.gatt.subscribe
    fixture.backend.gatt.subscribe = (path, request) => {
      const dispatch = originalSubscribe(path, request)
      return {
        ...dispatch,
        completion: dispatch.completion.then(subscription => ({
          ...subscription,
          terminal: { ...subscription.terminal, outcome: 'failed', cause: 'platform.failure' }
        }))
      }
    }

    await expect(
      settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    ).rejects.toMatchObject({ normalized: { code: 'protocol.violation' } })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(Number(fixture.backend.resourceCounters().subscriptionConsumers)).toBe(0)

    fixture.controller.queueCompletion('subscribe', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const abortController = new AbortController()
    const cancelled = database.subscribe(characteristic, subscriptionOptions(abortController.signal))
    await flushMicrotasks()
    abortController.abort()
    await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    const laterRead = database.read(characteristic, operation())
    await flushVirtual(fixture.controller)
    await expect(laterRead).resolves.toBeInstanceOf(Uint8Array)
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(0)
    expect(Number(fixture.backend.resourceCounters().subscriptionConsumers)).toBe(0)
    expect(manager.traces().some(record => record.transition === 'malformed-subscription-compensated')).toBe(true)
    fixture.backend.gatt.subscribe = originalSubscribe

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('rejects a malformed resolved unsubscribe terminal and retains cleanup ownership for retry', async () => {
    const { fixture, manager } = await createFixture()
    const { database, characteristic } = await connectedDatabase(fixture, manager)
    const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
    const originalUnsubscribe = fixture.backend.gatt.unsubscribe
    fixture.backend.gatt.unsubscribe = (backendSubscription, options) =>
      fixture.backend.createBackendOperationDispatch(options, async operationValue => ({
        correlation: operationValue.correlation,
        outcome: 'failed',
        cause: 'platform.failure'
      }))

    await expect(settle(fixture.controller, subscription.remove())).resolves.toMatchObject({
      state: 'release-failed',
      failures: [{ error: { code: 'protocol.violation' } }]
    })
    expect(Number(fixture.backend.resourceCounters().physicalCccdEnablements)).toBe(1)
    fixture.backend.gatt.unsubscribe = originalUnsubscribe
    await expect(settle(fixture.controller, subscription.remove())).resolves.toEqual({
      state: 'released',
      failures: []
    })

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('bounds connection release while retaining an unresolved quarantined operation for retry', async () => {
    jest.useFakeTimers()
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    const events = connection.events[Symbol.asyncIterator]()
    await events.next()
    const originalWrite = fixture.backend.gatt.write
    let resolveWrite = null
    let release = null
    fixture.backend.gatt.write = (_path, request) => ({
      completion: new Promise(resolve => {
        resolveWrite = () =>
          resolve({
            terminal: {
              correlation: request.operation.correlation,
              outcome: 'succeeded',
              cause: null
            },
            commitState: 'confirmed'
          })
      }),
      requestCancellation: async () => undefined
    })

    try {
      const write = database.write(characteristic, new Uint8Array(17), {
        ...operation(),
        mode: 'with-response'
      })
      await flushMicrotasks()
      release = connection.release()
      await expect(write).rejects.toMatchObject({ normalized: { code: 'operation.disconnected' } })
      await flushMicrotasks()
      let releaseSettled = false
      void release.then(() => {
        releaseSettled = true
      })

      jest.runOnlyPendingTimers()
      await flushVirtual(fixture.controller)

      expect(releaseSettled).toBe(true)
      await expect(release).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'operation-quarantine',
            error: { code: 'operation.timed-out', domain: 'cleanup' }
          }
        ]
      })
      let lifecycleEvent
      const lifecycleEventPromise = events.next()
      void lifecycleEventPromise.then(result => {
        lifecycleEvent = result
      })
      await flushMicrotasks()
      expect(lifecycleEvent).toMatchObject({
        done: false,
        value: { kind: 'value', value: { cause: 'released', current: 'disconnected' } }
      })
      await expect(events.next()).resolves.toMatchObject({
        done: false,
        value: { kind: 'terminal', reason: 'owner-released' }
      })
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(17)
      expect(Number(manager.localResourceCounters().connectionLeases)).toBe(1)

      resolveWrite()
      await expect(settle(fixture.controller, connection.release())).resolves.toEqual({
        state: 'released',
        failures: []
      })
      expectNoResources(fixture.backend.resourceCounters())
    } finally {
      fixture.backend.gatt.write = originalWrite
      if (resolveWrite !== null) resolveWrite()
      if (release !== null) await settle(fixture.controller, release)
      await events.return()
      jest.useRealTimers()
      await settle(fixture.controller, manager.destroy())
    }
  })

  test('bounds manager destruction while retaining an unresolved quarantined operation for retry', async () => {
    jest.useFakeTimers()
    const { fixture, manager } = await createFixture()
    const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
    const originalWrite = fixture.backend.gatt.write
    let resolveWrite = null
    let destruction = null
    fixture.backend.gatt.write = (_path, request) => ({
      completion: new Promise(resolve => {
        resolveWrite = () =>
          resolve({
            terminal: {
              correlation: request.operation.correlation,
              outcome: 'succeeded',
              cause: null
            },
            commitState: 'confirmed'
          })
      }),
      requestCancellation: async () => undefined
    })

    try {
      const write = database.write(characteristic, new Uint8Array(17), {
        ...operation(),
        mode: 'with-response'
      })
      await flushMicrotasks()
      destruction = manager.destroy()
      await expect(write).rejects.toMatchObject({ normalized: { code: 'operation.cancelled-by-destroy' } })
      await flushMicrotasks()
      let destructionSettled = false
      void destruction.then(() => {
        destructionSettled = true
      })

      jest.runOnlyPendingTimers()
      await flushVirtual(fixture.controller)

      expect(destructionSettled).toBe(true)
      await expect(destruction).resolves.toMatchObject({
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'operation-quarantine',
            error: { code: 'operation.timed-out', domain: 'cleanup' }
          }
        ]
      })
      expect(manager.state).toBe('failed')
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(17)
      expect(Number(manager.localResourceCounters().connectionLeases)).toBe(1)

      resolveWrite()
      await expect(settle(fixture.controller, manager.destroy())).resolves.toEqual({
        state: 'released',
        failures: []
      })
      expect(manager.state).toBe('destroyed')
      expectNoResources(fixture.backend.resourceCounters())
    } finally {
      fixture.backend.gatt.write = originalWrite
      if (resolveWrite !== null) resolveWrite()
      if (destruction !== null) await settle(fixture.controller, destruction)
      jest.useRealTimers()
      await settle(fixture.controller, manager.destroy())
    }
  })

  test.each([
    ['owning', 'late success', null],
    ['owning', 'late failure', 'platform.failure'],
    ['borrowing', 'late success', null],
    ['borrowing', 'late failure', 'platform.failure']
  ])(
    'keeps %s manager destruction pending with 17 owned write bytes until %s acknowledgement',
    async (ownerMode, _terminal, failure) => {
      const { fixture, manager, owner } = await createManagerFixture(ownerMode)
      const { connection, database, characteristic } = await connectedDatabase(fixture, manager)
      fixture.controller.queueCompletion('write', {
        delayMs: 10,
        failure,
        cancellable: false,
        deadlineOrder: 'completion-first'
      })
      const originalWrite = fixture.backend.gatt.write
      fixture.backend.gatt.write = (path, request) => {
        const dispatch = originalWrite(path, request)
        return {
          ...dispatch,
          requestCancellation: async () => ({
            handle: dispatch.handle,
            state: 'not-cancellable'
          })
        }
      }
      const coreConnection = connection.connection
      const originalLeaseRelease = coreConnection.lease.release.bind(coreConnection.lease)
      let localLeaseReleases = 0
      coreConnection.lease.release = async () => {
        localLeaseReleases += 1
        return { state: 'released', failures: [] }
      }
      const write = database.write(characteristic, new Uint8Array(17), {
        ...operation(),
        mode: 'with-response'
      })
      const writeTerminal = expect(write).rejects.toMatchObject({
        normalized: { code: 'operation.cancelled-by-destroy' }
      })
      fixture.controller.clock.advanceBy(0)
      await flushMicrotasks()
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(17)

      const destruction = manager.destroy()
      let destructionSettled = false
      void destruction.then(
        () => {
          destructionSettled = true
        },
        () => {
          destructionSettled = true
        }
      )
      await writeTerminal
      await flushMicrotasks()
      expect(destructionSettled).toBe(false)
      expect(manager.state).toBe('destroying')
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(17)
      expect(manager.traces().some(record => record.transition === 'quarantined')).toBe(true)

      fixture.controller.clock.advanceBy(9)
      await flushMicrotasks()
      expect(destructionSettled).toBe(false)
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(17)
      fixture.controller.clock.advanceBy(1)
      await expect(settle(fixture.controller, destruction)).resolves.toEqual({
        state: 'released',
        failures: []
      })
      expect(destructionSettled).toBe(true)
      expect(localLeaseReleases).toBe(1)
      expect(Number(manager.localResourceCounters().retainedByteBuffers)).toBe(0)
      expect(manager.traces().filter(record => record.transition === 'late-success')).toHaveLength(
        failure === null ? 1 : 0
      )
      expect(manager.traces().filter(record => record.transition === 'late-failure')).toHaveLength(
        failure === null ? 0 : 1
      )

      fixture.backend.gatt.write = originalWrite
      if (owner !== null) {
        await settle(fixture.controller, originalLeaseRelease())
        await settle(fixture.controller, owner.destroy())
      }
      expectNoResources(fixture.backend.resourceCounters())
    }
  )

  test('coalesces concurrent rediscovery waiters after a pending initial discovery', async () => {
    const { fixture, manager } = await createFixture()
    const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
    fixture.controller.queueCompletion('discover', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const initial = connection.connection.discover(operation())
    await flushMicrotasks()
    const first = connection.rediscoverGatt(operation(), 'manual-rediscovery')
    const second = connection.rediscoverGatt(operation(), 'manual-rediscovery')
    await flushMicrotasks()

    const firstOutcome = first.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )
    const secondOutcome = second.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )

    await settle(fixture.controller, initial)
    await flushVirtual(fixture.controller)

    const [firstResult, secondResult] = await Promise.all([firstOutcome, secondOutcome])
    expect(firstResult.state).toBe('fulfilled')
    expect(secondResult.state).toBe('fulfilled')
    firstResult.value.assertCurrent()
    secondResult.value.assertCurrent()
    expect(String(firstResult.value.path.databaseGeneration)).toBe(String(secondResult.value.path.databaseGeneration))
    expect(Number(manager.localResourceCounters().databaseSnapshots)).toBe(1)

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('aborting the rediscovery starter does not fail a waiter of the in-flight replacement', async () => {
    const { fixture, manager } = await createFixture()
    const { connection } = await connectedDatabase(fixture, manager)
    const originalDiscover = fixture.backend.gatt.discover.bind(fixture.backend.gatt)
    let releaseReplacement
    const replacementGate = new Promise(resolve => {
      releaseReplacement = resolve
    })
    const abortController = new AbortController()
    fixture.backend.gatt.discover = async (...args) => {
      abortController.abort()
      await replacementGate
      return originalDiscover(...args)
    }
    const starter = connection.connection.rediscoverGatt(operation(abortController.signal), 'manual-rediscovery')
    const waiter = connection.connection.rediscoverGatt(operation(), 'manual-rediscovery')
    await flushMicrotasks()

    const starterOutcome = starter.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )
    const waiterOutcome = waiter.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )
    releaseReplacement()
    await flushVirtual(fixture.controller)

    const [starterResult, waiterResult] = await Promise.all([starterOutcome, waiterOutcome])
    expect(starterResult.state).toBe('rejected')
    expect(starterResult.error).toMatchObject({ normalized: { code: 'operation.aborted' } })
    expect(waiterResult.state).toBe('fulfilled')
    waiterResult.value.assertCurrent()

    fixture.backend.gatt.discover = originalDiscover
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('a starter deadline does not fail a sibling rediscover waiter with no deadline', async () => {
    const { fixture, manager } = await createFixture()
    const { connection } = await connectedDatabase(fixture, manager)
    const originalDiscover = fixture.backend.gatt.discover.bind(fixture.backend.gatt)
    let releaseReplacement
    const replacementGate = new Promise(resolve => {
      releaseReplacement = resolve
    })
    fixture.backend.gatt.discover = async (...args) => {
      await replacementGate
      return originalDiscover(...args)
    }
    const starter = connection.connection.rediscoverGatt(
      operation(null, deadline(manager.monotonicNow() + 20)),
      'manual-rediscovery'
    )
    const waiter = connection.connection.rediscoverGatt(operation(), 'manual-rediscovery')
    await flushMicrotasks()

    const waiterOutcome = waiter.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )
    await expect(starter).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    releaseReplacement()
    await flushVirtual(fixture.controller)

    const waiterResult = await waiterOutcome
    expect(waiterResult.state).toBe('fulfilled')
    waiterResult.value.assertCurrent()

    fixture.backend.gatt.discover = originalDiscover
    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('discover joiner follows a replacement rediscovery instead of asserting a stale snapshot', async () => {
    const { fixture, manager } = await createFixture()
    const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
    fixture.controller.queueCompletion('discover', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const initial = connection.connection.discover(operation())
    await flushMicrotasks()
    const rediscovery = connection.rediscoverGatt(operation(), 'manual-rediscovery')
    const joiner = connection.connection.discover(operation())
    await flushMicrotasks()

    const joinerOutcome = joiner.then(
      value => ({ state: 'fulfilled', value }),
      error => ({ state: 'rejected', error })
    )
    await settle(fixture.controller, initial)
    await flushVirtual(fixture.controller)

    const joinerResult = await joinerOutcome
    expect(joinerResult.state).toBe('fulfilled')
    joinerResult.value.assertCurrent()
    const replacement = await settle(fixture.controller, rediscovery)
    replacement.assertCurrent()
    expect(String(joinerResult.value.path.databaseGeneration)).toBe(String(replacement.path.databaseGeneration))

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('keeps a rediscovery joiner abort local while the other waiter receives the replacement', async () => {
    const { fixture, manager } = await createFixture()
    const connection = await settle(fixture.controller, manager.connect(peer(), operation()))
    fixture.controller.queueCompletion('discover', {
      delayMs: 10,
      failure: null,
      cancellable: false,
      deadlineOrder: 'completion-first'
    })
    const initial = connection.connection.discover(operation())
    await flushMicrotasks()
    const abortController = new AbortController()
    const cancelled = connection.rediscoverGatt(operation(abortController.signal), 'manual-rediscovery')
    const remaining = connection.rediscoverGatt(operation(), 'manual-rediscovery')
    await flushMicrotasks()
    abortController.abort()

    await expect(cancelled).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await settle(fixture.controller, initial)
    const replacement = await settle(fixture.controller, remaining)
    replacement.assertCurrent()

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })

  test('service-changed concurrent rediscovery joins one replacement without stale-handle rejection', async () => {
    const { fixture, manager } = await createFixture()
    const { connection } = await connectedDatabase(fixture, manager)
    fixture.controller.triggerServicesChanged(connection.peerId)
    await flushMicrotasks()
    const first = connection.rediscoverGatt(operation(), 'service-changed')
    const second = connection.rediscoverGatt(operation(), 'service-changed')

    const [firstDatabase, secondDatabase] = await Promise.all([
      settle(fixture.controller, first),
      settle(fixture.controller, second)
    ])
    firstDatabase.assertCurrent()
    secondDatabase.assertCurrent()
    expect(String(firstDatabase.path.databaseGeneration)).toBe(String(secondDatabase.path.databaseGeneration))

    await settle(fixture.controller, manager.destroy())
    expectNoResources(fixture.backend.resourceCounters())
  })
})
