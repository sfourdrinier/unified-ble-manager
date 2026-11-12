const {
  attachBleBackend,
  BleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../src/manager/ble-manager')
const { createDeterministicTestBackend } = require('../../src/testing/deterministic/deterministic-test-backend')
const { capacity, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function operation() {
  return { signal: null, deadline: null }
}

function subscriptionOptions() {
  return {
    ...operation(),
    delivery: {
      itemCapacity: capacity(4),
      byteCapacity: capacity(1024),
      reservedControlCapacity: capacity(1),
      overflowPolicy: 'drop-oldest'
    }
  }
}

async function settle(controller, promise) {
  let settled = false
  promise.then(
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

async function createFixture(reserveTerminalEvidence) {
  const fixture = createDeterministicTestBackend()
  fixture.backend.gatt.reserveTerminalEvidence = reserveTerminalEvidence
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const manager = await BleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('terminal-evidence-client', 'client', 'deterministic:terminal-evidence-client'),
      managerId: opaqueId('terminal-evidence-manager', 'manager', 'deterministic:terminal-evidence-manager'),
      ownerMode: 'owning'
    },
    authority,
    { ...DEFAULT_BLE_MANAGER_OPTIONS, terminalEvidence: { maximumCanonicalBytes: 512 } }
  )
  const connection = await settle(
    fixture.controller,
    manager.connect(opaqueId('deterministic-peer', 'peer', 'deterministic'), operation())
  )
  const database = await settle(fixture.controller, connection.discover(operation()))
  const characteristic = (await database.snapshot()).characteristics[0].path
  return { fixture, manager, database, characteristic }
}

function terminalSource() {
  return new CoreBoundedStream(
    { itemCapacity: capacity(4), byteCapacity: capacity(1024), reservedControlCapacity: capacity(1) },
    'drop-oldest'
  )
}

test('releases a pre-activation subscription evidence reservation after normal unsubscribe', async () => {
  const release = jest.fn(async () => {})
  const reserve = jest.fn(async request => ({
    maximumBytes: request.maximumBytes,
    commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }),
    release
  }))
  const { fixture, manager, database, characteristic } = await createFixture(reserve)

  const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
  expect(reserve).toHaveBeenCalledWith(expect.objectContaining({ producer: 'subscription' }))

  await expect(settle(fixture.controller, subscription.remove())).resolves.toEqual({ state: 'released', failures: [] })
  expect(release).toHaveBeenCalledTimes(1)
  await settle(fixture.controller, manager.destroy())
})

test('compensates a reservation when enable fails before a subscription is activated', async () => {
  const release = jest.fn(async () => {})
  const reserve = jest.fn(async request => ({
    maximumBytes: request.maximumBytes,
    commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }),
    release
  }))
  const { fixture, manager, database, characteristic } = await createFixture(reserve)
  fixture.controller.queueCompletion('subscribe', {
    delayMs: 1,
    failure: 'platform.failure',
    cancellable: false,
    deadlineOrder: 'completion-first'
  })

  await expect(
    settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
  ).rejects.toMatchObject({
    normalized: { code: 'platform.failure' }
  })
  expect(release).toHaveBeenCalledTimes(1)
  await settle(fixture.controller, manager.destroy())
})

test('retains a failed terminal-evidence release for an explicit retry', async () => {
  let releases = 0
  const reserve = jest.fn(async request => ({
    maximumBytes: request.maximumBytes,
    commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }),
    release: async () => {
      releases += 1
      if (releases === 1) throw new Error('evidence store unavailable')
    }
  }))
  const { fixture, manager, database, characteristic } = await createFixture(reserve)
  const subscription = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))

  await expect(settle(fixture.controller, subscription.remove())).resolves.toMatchObject({ state: 'release-failed' })
  await expect(settle(fixture.controller, subscription.remove())).resolves.toEqual({ state: 'released', failures: [] })
  expect(releases).toBe(2)
  expect(manager.traces().some(record => record.transition === 'terminal-evidence-release-failed')).toBe(true)
  await settle(fixture.controller, manager.destroy())
})

test('binds a recorded subscription to its exact pre-activation evidence scope', async () => {
  let reservationRequest
  const release = jest.fn(async () => {})
  const reserve = jest.fn(async request => {
    reservationRequest = request
    return {
      maximumBytes: request.maximumBytes,
      commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }),
      release
    }
  })
  const { fixture, manager, database, characteristic } = await createFixture(reserve)
  const subscribe = fixture.backend.gatt.subscribe.bind(fixture.backend.gatt)
  fixture.backend.gatt.subscribeRecorded = jest.fn((path, request) => {
    const dispatch = subscribe(path, request)
    return {
      ...dispatch,
      completion: dispatch.completion.then(subscription => ({ ...subscription, changed: terminalSource() }))
    }
  })

  const subscription = await settle(
    fixture.controller,
    database.subscribeRecorded(characteristic, {
      ...subscriptionOptions(),
      recording: { id: 'recorded-session', maxBytes: 1048576, maxRecords: 1 }
    })
  )
  expect(fixture.backend.gatt.subscribeRecorded).toHaveBeenCalledWith(
    characteristic,
    expect.objectContaining({ terminalEvidenceScope: reservationRequest.scope })
  )
  await expect(settle(fixture.controller, subscription.remove())).resolves.toEqual({ state: 'released', failures: [] })
  expect(release).toHaveBeenCalledTimes(1)
  await settle(fixture.controller, manager.destroy())
})

test('keeps evidence through a source terminal until every shared consumer has read it', async () => {
  const release = jest.fn(async () => {})
  let reservationRequest
  const reserve = jest.fn(async request => {
    reservationRequest = request
    return {
      maximumBytes: request.maximumBytes,
      commit: async () => ({ kind: 'refused', reason: 'storage-unavailable' }),
      release
    }
  })
  const { fixture, manager, database, characteristic } = await createFixture(reserve)
  const source = terminalSource()
  const subscribe = fixture.backend.gatt.subscribe.bind(fixture.backend.gatt)
  fixture.backend.gatt.subscribe = (path, request) => {
    const dispatch = subscribe(path, request)
    return {
      ...dispatch,
      completion: dispatch.completion.then(subscription => ({ ...subscription, notifications: source }))
    }
  }

  const first = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
  const second = await settle(fixture.controller, database.subscribe(characteristic, subscriptionOptions()))
  source.finishWithReason('source-failed', null, {
    producer: 'subscription',
    ref: 'terminal-ref',
    byteLength: 1,
    digest: 'sha256:terminal',
    formatVersion: 1,
    durability: 'committed',
    scope: reservationRequest.scope
  })

  const firstValues = first.values[Symbol.asyncIterator]()
  const secondValues = second.values[Symbol.asyncIterator]()
  await expect(firstValues.next()).resolves.toMatchObject({
    value: { kind: 'terminal', evidence: { ref: 'terminal-ref' } }
  })
  expect(release).not.toHaveBeenCalled()
  await expect(secondValues.next()).resolves.toMatchObject({
    value: { kind: 'terminal', evidence: { ref: 'terminal-ref' } }
  })
  await Promise.resolve()
  expect(release).toHaveBeenCalledTimes(1)
  await settle(fixture.controller, manager.destroy())
})
