// __tests__/public-gatt-object-model.test.js

const {
  attachBleBackend,
  BleManager: InternalBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../src/manager/ble-manager')
const { createPublicBleManager, publicConnectionEvents } = require('../src/public/ble-manager')
const { createDeterministicTestBackend } = require('../src/testing/deterministic/deterministic-test-backend')
const { opaqueId, version, versionRange } = require('../src/backend-contract/primitives')
const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { driveVirtualClock } = require('./helpers/async')

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

async function createPublicFixture(observedDeliveryOverride, onNativeSubscribe, notificationReceiptOverride) {
  const fixture = createDeterministicTestBackend()
  if (
    observedDeliveryOverride !== undefined ||
    onNativeSubscribe !== undefined ||
    notificationReceiptOverride !== undefined
  ) {
    const nativeGatt = fixture.backend.gatt
    fixture.backend.gatt = {
      ...nativeGatt,
      subscribe(path, request) {
        onNativeSubscribe?.(path, request)
        const dispatch = nativeGatt.subscribe(path, request)
        return {
          ...dispatch,
          completion: dispatch.completion.then(subscription => {
            return {
              ...(observedDeliveryOverride === undefined
                ? subscription
                : { ...subscription, observedDelivery: observedDeliveryOverride }),
              ...(notificationReceiptOverride === undefined
                ? {}
                : {
                    notifications: {
                      ...subscription.notifications,
                      async *[Symbol.asyncIterator]() {
                        for await (const item of subscription.notifications) {
                          if (item.kind === 'value') {
                            yield { ...item, value: { ...item.value, receipt: notificationReceiptOverride } }
                          } else yield item
                        }
                      }
                    }
                  })
            }
          })
        }
      }
    }
  }
  const attachedBackend = await attachBleBackend(fixture.backend, compatibility())
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const internal = await InternalBleManager.create(
    {
      attachedBackend,
      clientId: opaqueId('public-gatt-client', 'client', 'public-gatt'),
      managerId: opaqueId('public-gatt-manager', 'manager', 'public-gatt'),
      ownerMode: 'owning'
    },
    authority,
    DEFAULT_BLE_MANAGER_OPTIONS
  )
  const manager = await createPublicBleManager(internal, () => Number(fixture.controller.clock.now()))
  return { fixture, manager }
}

async function settle(fixture, promise) {
  return driveVirtualClock(fixture.controller.clock, promise, 'public GATT operation')
}

async function connectAndDiscover(fixture, manager) {
  const connection = await settle(fixture, manager.connect('deterministic-peer'))
  const database = await settle(fixture, connection.discover())
  return { connection, database }
}

function emptyPublicGattSnapshot() {
  return {
    path: {
      attachmentId: 'projection-attachment',
      peerId: 'projection-peer',
      connectionId: 'projection-connection',
      ownerLeaseId: 'projection-lease',
      connectionGeneration: 'projection-connection-generation',
      databaseId: 'projection-database',
      databaseGeneration: 'projection-database-generation'
    },
    services: [],
    characteristics: [],
    descriptors: []
  }
}

function orderedPublicGattSnapshot() {
  const path = emptyPublicGattSnapshot().path
  const servicePath = { ...path, serviceUuid: '180f', serviceOccurrence: '0' }
  return {
    path,
    services: [{ path: servicePath, primary: true, includedServices: [] }],
    characteristics: [{
      path: { ...servicePath, characteristicUuid: '2a19', characteristicOccurrence: '0', validity: 'current' },
      properties: { read: true, writeWithResponse: true, writeWithoutResponse: true, notify: true, indicate: false }
    }],
    descriptors: []
  }
}

describe('stable public GATT object model (PR3 TDD)', () => {
  test('ordered reclaim snapshots nested public error metadata and rejects malformed producer evidence', async () => {
    const { createPublicGattDatabase } = require('../src/public/gatt')
    const bytes = new Uint8Array([7])
    const goodError = {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'ordered.reclaim',
      retryability: 'never',
      platform: { domain: 'native', code: 'E', safeMessage: 'failed', metadata: { bytes } }
    }
    const good = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingReclaim: () => ({ start: async () => ({ state: 'failed', mappingCommitted: false, error: goodError }) })
    })
    const receipt = await good.reserveOrderedRecordingReclaim({ kind: 'ordered-recording-recovery' }).start()
    expect(receipt).toMatchObject({ state: 'failed', error: { platform: { metadata: { bytes: new Uint8Array([7]) } } } })
    bytes[0] = 99
    expect(receipt.error.platform.metadata.bytes).toEqual(new Uint8Array([7]))

    const markerAndStart = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingMarker: () => ({
        status: async () => ({ state: 'not-admitted', error: goodError }),
        cancel: async () => ({ state: 'not-admitted', error: goodError }),
        start: async () => ({ state: 'not-admitted', error: goodError }),
        forget: async () => undefined
      }),
      reserveOrderedRecordingStart: () => ({
        status: async () => ({
          state: 'result',
          result: {
            kind: 'prepare-failed',
            leaseCleanup: { state: 'release-failed', failures: [{ resourceKind: 'recording', error: goodError }] },
            receipt: { version: 2, state: 'prepare-failed', recordingId: 'r', sourceStarted: false, journalEffect: 'none', error: { kind: 'source', source: { code: 'platform.failure', domain: 'platform', operation: 'ordered.reclaim', detail: null, platformDomain: null, platformCode: null, platformMessage: null, platformMetadata: {} } } }
          }
        }),
        start: async () => { throw new Error('not used') },
        retire: async () => undefined
      })
    })
    await expect(markerAndStart.reserveOrderedRecordingMarker('r').status()).resolves.toMatchObject({
      state: 'not-admitted',
      error: { platform: { metadata: { bytes: new Uint8Array([99]) } } }
    })
    await expect(markerAndStart.reserveOrderedRecordingStart().status()).resolves.toMatchObject({
      state: 'result',
      result: { kind: 'prepare-failed', leaseCleanup: { state: 'release-failed' } }
    })

    const failureMetadata = { note: { kind: 'text', value: 'original' } }
    const startedReceipt = {
      version: 2, recordingId: 'recording', collectionId: '1', state: 'active', sourceGeneration: { kind: 'counter', value: '2' },
      members: [{ memberId: '0', acquisition: 'acquired', delivery: 'notification', cleanupReleased: true, cleanupError: null }],
      setupFailure: null, fenceFailure: null,
      error: { kind: 'source', source: { code: 'source.failure', domain: 'source', operation: 'start', detail: null, platformDomain: 'native', platformCode: 'E', platformMessage: null, platformMetadata: failureMetadata } },
      inspectionError: null, driverError: null
    }
    const started = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingStart: () => {
        let calls = 0
        return {
        status: async () => ({ state: 'result', result: calls++ === 0 ? { kind: 'started', receipt: startedReceipt } : { kind: 'owner-failed', recordingId: 'recording', collectionId: '1', error: startedReceipt.error } }),
        start: async () => { throw new Error('not used') },
        retire: async () => undefined
        }
      }
    })
    const startedHandle = started.reserveOrderedRecordingStart()
    const startedStatus = await startedHandle.status()
    const ownerFailedStatus = await startedHandle.status()
    failureMetadata.note.value = 'mutated'
    startedReceipt.sourceGeneration.value = 'mutated'
    expect(startedStatus).toMatchObject({ state: 'result', result: { kind: 'started', receipt: { sourceGeneration: { value: '2' }, error: { source: { platformMetadata: { note: { value: 'original' } } } } } } })
    expect(ownerFailedStatus).toMatchObject({ state: 'result', result: { kind: 'owner-failed', error: { source: { platformMetadata: { note: { value: 'original' } } } } } })

    const malformedStart = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingStart: () => ({ status: async () => ({ state: 'result', result: { kind: 'bogus' } }), retire: async () => undefined })
    })
    await expect(malformedStart.reserveOrderedRecordingStart().status()).rejects.toMatchObject({ code: 'protocol.malformed' })

    const malformedMetadata = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingStart: () => ({
        status: async () => ({
          state: 'result',
          result: {
            kind: 'started',
            receipt: {
              ...startedReceipt,
              error: {
                ...startedReceipt.error,
                source: {
                  ...startedReceipt.error.source,
                  platformMetadata: { bad: { kind: 'bogus', value: 'x' } }
                }
              }
            }
          }
        }),
        retire: async () => undefined
      })
    })
    await expect(malformedMetadata.reserveOrderedRecordingStart().status()).rejects.toMatchObject({ code: 'protocol.malformed' })

    const malformed = await createPublicGattDatabase({
      snapshot: async () => emptyPublicGattSnapshot(),
      reserveOrderedRecordingReclaim: () => ({
        start: async () => ({
          state: 'failed',
          mappingCommitted: false,
          error: { ...goodError, platform: { ...goodError.platform, metadata: new Date() } }
        })
      })
    })
    await expect(malformed.reserveOrderedRecordingReclaim({ kind: 'ordered-recording-recovery' }).start()).rejects.toMatchObject({
      code: 'protocol.malformed'
    })
  })

  test('ordered public stream snapshots nested source evidence on iteration, cancellation, cleanup, and failed setup', async () => {
    const { createPublicGattDatabase } = require('../src/public/gatt')
    const bytes = new Uint8Array([7])
    const membership = { member: 'native-member', epoch: '1' }
    const sourceGeneration = { kind: 'counter', value: '2' }
    const item = {
      kind: 'value', collectionId: 'collection', connectionGeneration: 'connection', databaseGeneration: 'database',
      memberId: 'battery', ordinal: '1', sourceGeneration, sourceMembership: membership, value: bytes,
      delivery: 'notification', provenance: 'notification',
      receipt: { receivedAtUtcMs: 1, receivedAtMonotonicMs: 2, clockId: 'native-clock', origin: 'native' }
    }
    const cleanup = {
      group: { state: 'released', failures: [] }, members: [],
      discarded: {
        values: '1', bytes: '1', controls: '1', uncertainTail: false,
        losses: [{ kind: 'loss', scope: { kind: 'member', memberId: 'battery' }, reason: 'overflow', lostValues: '1', lostBytes: '1', lostSourceRecords: '1' }],
        markers: [{ kind: 'marker', collectionId: 'collection', connectionGeneration: 'connection', databaseGeneration: 'database', sourceGeneration, sourceMembership: membership, markerId: 'marker', ordinal: '2' }],
        trustedValues: [{ memberId: 'battery', ordinal: '1', sourceMembership: membership, bytes: '1' }]
      }
    }
    const iterator = {
      next: async () => ({ done: false, value: item }),
      return: async () => ({ done: false, value: item }),
      throw: async () => ({ done: false, value: item })
    }
    const stream = {
      limits: { itemCapacity: 2, byteCapacity: 64, reservedControlCapacity: 1 },
      [Symbol.asyncIterator]: () => iterator,
      close: async () => cleanup,
      discard: async () => cleanup
    }
    const database = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      subscribeOrdered: async members => ({
        state: 'ready', members: members.map(member => ({ ...member, requestedDelivery: member.deliveryMode, effectiveDelivery: 'notification' })), stream
      })
    })
    const characteristic = database.service('180f').characteristic('2a19')
    const setup = await database.subscribeOrdered([{ memberId: 'battery', characteristic }])
    expect(setup.state).toBe('ready')
    if (setup.state !== 'ready') throw new Error('expected ready setup')
    const publicIterator = setup.stream[Symbol.asyncIterator]()
    const first = await publicIterator.next()
    const fromReturn = await publicIterator.return()
    const fromThrow = await publicIterator.throw(new Error('cancel'))
    const publicCleanup = await setup.stream.discard()
    bytes[0] = 99
    membership.epoch = 'mutated'
    sourceGeneration.value = 'mutated'
    cleanup.discarded.losses[0].scope.memberId = 'mutated'
    expect(first.value).toMatchObject({ value: new Uint8Array([7]), sourceGeneration: { value: '2' }, sourceMembership: { epoch: '1' } })
    expect(fromReturn.value).toMatchObject({ value: new Uint8Array([7]), sourceGeneration: { value: '2' } })
    expect(fromThrow.value).toMatchObject({ value: new Uint8Array([7]), sourceMembership: { epoch: '1' } })
    expect(publicCleanup.discarded).toMatchObject({ losses: [{ scope: { memberId: 'battery' } }], markers: [{ sourceGeneration: { value: '2' }, sourceMembership: { epoch: '1' } }], trustedValues: [{ sourceMembership: { epoch: '1' } }] })

    const failedBytes = new Uint8Array([8])
    const failed = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      subscribeOrdered: async () => ({
        state: 'failed', error: { code: 'platform.failure', domain: 'platform', operation: 'ordered', retryability: 'never', platform: null },
        members: [], observations: [{ ...item, value: failedBytes }], uncertainTail: false, cleanup, stream,
        close: async () => { throw require('../src/backend-contract/errors').contractError('platform.failure', 'platform', 'ordered.close') }
      })
    })
    const failedCharacteristic = failed.service('180f').characteristic('2a19')
    const failedSetup = await failed.subscribeOrdered([{ memberId: 'battery', characteristic: failedCharacteristic }])
    expect(failedSetup.state).toBe('failed')
    if (failedSetup.state !== 'failed') throw new Error('expected failed setup')
    failedBytes[0] = 88
    expect(failedSetup.observations).toMatchObject([{ kind: 'value', value: new Uint8Array([8]) }])
    await expect(failedSetup.close()).rejects.toMatchObject({ code: 'platform.failure' })

    const malformedItem = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      subscribeOrdered: async members => ({ state: 'ready', members: members.map(member => ({ ...member, requestedDelivery: member.deliveryMode, effectiveDelivery: 'notification' })), stream: { ...stream, [Symbol.asyncIterator]: () => ({ next: async () => ({ done: false, value: { kind: 'bogus' } }) }) } })
    })
    const malformedItemSetup = await malformedItem.subscribeOrdered([{ memberId: 'battery', characteristic: malformedItem.service('180f').characteristic('2a19') }])
    if (malformedItemSetup.state !== 'ready') throw new Error('expected ready setup')
    await expect(malformedItemSetup.stream[Symbol.asyncIterator]().next()).rejects.toMatchObject({ code: 'protocol.malformed' })

    let stale = false
    const markerStart = jest.fn(async () => ({ state: 'pending' }))
    const markerDatabase = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      assertCurrent: () => { if (stale) throw require('../src/backend-contract/errors').contractError('gatt.stale-handle', 'gatt', 'marker') },
      reserveOrderedRecordingMarker: () => ({ start: markerStart, status: async () => ({ state: 'bogus' }), cancel: async () => ({ state: 'bogus' }), forget: async () => undefined })
    })
    const markerCharacteristic = markerDatabase.service('180f').characteristic('2a19')
    const marker = markerDatabase.reserveOrderedRecordingMarker('recording')
    await marker.start(markerCharacteristic, new Uint8Array([1]), 'with-response')
    stale = true
    await expect(marker.start(markerCharacteristic, new Uint8Array([1]), 'with-response')).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    expect(markerStart).toHaveBeenCalledTimes(1)
    stale = false
    await expect(marker.status()).rejects.toMatchObject({ code: 'protocol.malformed' })
    await expect(marker.cancel()).rejects.toMatchObject({ code: 'protocol.malformed' })

    let pendingRetryStale = false
    const pendingRetryStart = jest.fn(async () => ({ state: 'pending' }))
    const pendingRetryDatabase = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      assertCurrent: () => {
        if (pendingRetryStale)
          throw require('../src/backend-contract/errors').contractError('gatt.stale-handle', 'gatt', 'marker')
      },
      reserveOrderedRecordingMarker: () => ({
        start: pendingRetryStart,
        status: async () => ({ state: 'pending' }),
        cancel: async () => ({ state: 'pending' }),
        forget: async () => undefined
      })
    })
    const pendingRetryCharacteristic = pendingRetryDatabase.service('180f').characteristic('2a19')
    const pendingRetryMarker = pendingRetryDatabase.reserveOrderedRecordingMarker('recording')
    await pendingRetryMarker.start(pendingRetryCharacteristic, new Uint8Array([1]), 'with-response')
    pendingRetryStale = true
    await expect(
      pendingRetryMarker.start(pendingRetryCharacteristic, new Uint8Array([1]), 'with-response')
    ).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    expect(pendingRetryStart).toHaveBeenCalledTimes(1)

    const admittedRetryStart = jest.fn(async () => ({
      state: 'admitted', ordinal: '1', write: { state: 'settled', error: null }
    }))
    let admittedRetryStale = false
    const admittedRetryDatabase = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      assertCurrent: () => {
        if (admittedRetryStale)
          throw require('../src/backend-contract/errors').contractError('gatt.stale-handle', 'gatt', 'marker')
      },
      reserveOrderedRecordingMarker: () => ({
        start: admittedRetryStart,
        status: async () => ({ state: 'admitted', ordinal: '1', write: { state: 'settled', error: null } }),
        cancel: async () => ({ state: 'admitted', ordinal: '1', write: { state: 'settled', error: null } }),
        forget: async () => undefined
      })
    })
    const admittedRetryCharacteristic = admittedRetryDatabase.service('180f').characteristic('2a19')
    const admittedRetryMarker = admittedRetryDatabase.reserveOrderedRecordingMarker('recording')
    await admittedRetryMarker.start(admittedRetryCharacteristic, new Uint8Array([1]), 'with-response')
    admittedRetryStale = true
    await expect(
      admittedRetryMarker.start(admittedRetryCharacteristic, new Uint8Array([1]), 'with-response')
    ).resolves.toMatchObject({ state: 'admitted', ordinal: '1' })
    expect(admittedRetryStart).toHaveBeenCalledTimes(1)
    await expect(
      admittedRetryMarker.start(admittedRetryCharacteristic, new Uint8Array([2]), 'without-response')
    ).rejects.toMatchObject({ code: 'lifecycle.invalid-state' })
    await expect(
      admittedRetryMarker.start(admittedRetryCharacteristic, new Int8Array([1]), 'with-response')
    ).rejects.toMatchObject({ code: 'argument.invalid' })
    expect(admittedRetryStart).toHaveBeenCalledTimes(1)

    const malformedMarkerWrite = await createPublicGattDatabase({
      snapshot: async () => orderedPublicGattSnapshot(), monotonicNow: () => 0,
      reserveOrderedRecordingMarker: () => ({ start: async () => ({ state: 'admitted', ordinal: '1', write: { state: 'bogus', error: null } }), status: async () => ({ state: 'idle' }), cancel: async () => ({ state: 'idle' }), forget: async () => undefined })
    })
    const malformedMarker = malformedMarkerWrite.reserveOrderedRecordingMarker('recording')
    await expect(malformedMarker.start(malformedMarkerWrite.service('180f').characteristic('2a19'), new Uint8Array([1]), 'with-response')).rejects.toMatchObject({ code: 'protocol.malformed' })
  })

  test('service graph preserves secondary status, duplicate occurrence edges and unknown metadata', async () => {
    const { createPublicGattDatabase } = require('../src/public/gatt')
    const { fixture, manager } = await createPublicFixture()
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const original = database.snapshot()
    const first = original.services[0]
    const path = {
      attachmentId: 'graph-attachment',
      peerId: 'graph-peer',
      connectionId: 'graph-connection',
      ownerLeaseId: 'graph-lease',
      connectionGeneration: 'graph-connection-generation',
      databaseId: 'graph-database',
      databaseGeneration: database.generation
    }
    const servicePath = { ...path, serviceUuid: first.uuid }
    const graph = {
      path,
      characteristics: [],
      descriptors: [],
      services: [
        {
          path: { ...servicePath, serviceOccurrence: '0' },
          primary: true,
          includedServices: [{ uuid: first.uuid, occurrence: '1' }]
        },
        { path: { ...servicePath, serviceOccurrence: '1' }, primary: false, includedServices: [] },
        { path: { ...servicePath, serviceOccurrence: '2' }, primary: null, includedServices: null }
      ]
    }
    const publicGraph = await createPublicGattDatabase({ snapshot: async () => graph, assertCurrent: () => undefined })
    expect(publicGraph.services[0].primary).toBe(true)
    expect(publicGraph.services[0].includedServices).toEqual([{ uuid: first.uuid, occurrence: 1 }])
    expect(publicGraph.services[1].primary).toBe(false)
    expect(publicGraph.services[1].includedServices).toEqual([])
    expect(publicGraph.services[2].primary).toBeNull()
    expect(publicGraph.services[2].includedServices).toBeNull()
    const unresolved = {
      ...graph,
      services: [{ ...graph.services[0], includedServices: [{ uuid: first.uuid, occurrence: '99' }] }]
    }
    await expect(createPublicGattDatabase({ snapshot: async () => unresolved })).rejects.toMatchObject({
      code: 'protocol.violation'
    })
    await settle(fixture, connection.release())
    await settle(fixture, manager.destroy())
  })
  test('subscription receipt uses settled native delivery, including unknown, across shared consumers', async () => {
    const { fixture, manager } = await createPublicFixture('unknown')
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const first = await settle(fixture, characteristic.subscribe({ delivery: 'prefer-indication' }))
    const second = await settle(fixture, characteristic.subscribe({ delivery: 'prefer-indication' }))
    expect(first.requestedDelivery).toBe('prefer-indication')
    expect(first.effectiveDelivery).toBe('unknown')
    expect(second.effectiveDelivery).toBe('unknown')
    await settle(fixture, first.remove())
    await settle(fixture, second.remove())
    await settle(fixture, connection.release())
    await settle(fixture, manager.destroy())
  })

  test('dual-property subscriptions report the native mode and an impossible hard requirement stops before backend effects', async () => {
    const nativeSubscribe = jest.fn()
    const { fixture, manager } = await createPublicFixture(undefined, nativeSubscribe)
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const dual = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const notified = await settle(fixture, dual.subscribe({ delivery: 'prefer-notification' }))
    const indicated = await settle(fixture, dual.subscribe({ delivery: 'require-indication' }))
    expect(notified.effectiveDelivery).toBe('notification')
    expect(indicated.effectiveDelivery).toBe('indication')
    const before = nativeSubscribe.mock.calls.length
    const notifyOnly = database.service('180f', { occurrence: 1 }).characteristic('2a19', { occurrence: 0 })
    await expect(notifyOnly.subscribe({ delivery: 'require-indication' })).rejects.toMatchObject({
      code: 'gatt.property-not-supported'
    })
    expect(nativeSubscribe).toHaveBeenCalledTimes(before)
    await settle(fixture, notified.remove())
    await settle(fixture, indicated.remove())
    await settle(fixture, connection.release())
    await settle(fixture, manager.destroy())
  })

  test('broadcasts public lifecycle events to independent consumers', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const first = events[Symbol.asyncIterator]()
    const second = events[Symbol.asyncIterator]()
    const firstNext = first.next()
    const secondNext = second.next()
    source.emit(
      {
        kind: 'connection-lifecycle',
        attachment: {},
        attachmentId: 'attachment-1',
        peerId: 'peer-1',
        connectionId: 'connection-1',
        connectionGeneration: 'generation-1',
        ownerLeaseId: 'lease-1',
        sequence: 1,
        backendIngressOrdinal: null,
        previous: 'connecting',
        current: 'connected',
        cause: 'connected'
      },
      1
    )

    await expect(firstNext).resolves.toMatchObject({ value: { current: 'connected' } })
    await expect(secondNext).resolves.toMatchObject({ value: { current: 'connected' } })
    await first.return()
    await second.return()
  })

  test('closes lifecycle iterators created after the source has terminated', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const first = events[Symbol.asyncIterator]()
    source.closeWithReason('closed')
    await expect(first.next()).resolves.toMatchObject({ done: true })

    const second = events[Symbol.asyncIterator]()
    await expect(second.next()).resolves.toMatchObject({ done: true })
  })

  test('removes lifecycle subscribers when the connection event source terminates', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const events = publicConnectionEvents(source)
    const iterator = events[Symbol.asyncIterator]()

    source.closeWithReason('closed')
    await expect(iterator.next()).resolves.toMatchObject({ done: true })
    expect(events.subscribers.size).toBe(0)

    await iterator.return()
  })

  test('reports failed scan cleanup instead of a false stopped state', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({
        observations: source,
        stop: async () => ({
          state: 'release-failed',
          failures: [
            {
              resourceKind: 'scan',
              error: {
                code: 'platform.failure',
                domain: 'scan',
                operation: 'public-gatt-object-model.scan-stop',
                platform: null,
                retryability: 'never'
              }
            }
          ]
        })
      })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const scan = await manager.scan()
    const stateIterator = scan.state[Symbol.asyncIterator]()

    await expect(scan.stop()).resolves.toMatchObject({ state: 'release-failed' })
    const states = []
    for (;;) {
      const next = await stateIterator.next()
      if (next.done) break
      states.push(next.value.state)
    }
    expect(states).toEqual(['active', 'stopping', 'failed'])
  })

  test('preserves find abort and timeout causes when scan teardown closes the stream', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let now = 0
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({ observations: source, stop: async () => ({ state: 'released', failures: [] }) })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => now)
    const abortController = new AbortController()
    const aborted = manager.find({ signal: abortController.signal })
    await Promise.resolve()
    abortController.abort()
    source.closeWithReason('owner-released')
    await expect(aborted).rejects.toMatchObject({ code: 'operation.aborted' })

    const timedOut = manager.find({ timeoutMs: 5 })
    await Promise.resolve()
    now = 5
    source.closeWithReason('owner-released')
    await expect(timedOut).rejects.toMatchObject({ code: 'operation.timed-out' })
  })

  test('find keeps selection local and does not forward it as a scan option', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const stop = jest.fn(async () => ({ state: 'released', failures: [] }))
    const internal = {
      identity: null,
      attachedBackend: undefined,
      supports: () => true,
      scan: jest.fn(async () => ({ observations: source, stop })),
      connect: jest.fn(),
      destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
    }
    const manager = await createPublicBleManager(internal, () => 0)
    const found = manager.find({ select: 'first' })
    source.emit(
      {
        peerId: 'peer-1',
        localName: 'Sensor',
        rssi: -42,
        serviceUuids: [],
        manufacturerData: [],
        serviceData: []
      },
      1
    )

    await expect(found).resolves.toMatchObject({ id: 'peer-1', name: 'Sensor', rssi: -42 })
    expect(internal.scan).toHaveBeenCalledWith(expect.not.objectContaining({ select: 'first' }))
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('find keeps its historical observation budget by default and honours a caller-supplied one', async () => {
    const createFindFixture = () => {
      const source = new CoreBoundedStream(
        { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
        'drop-oldest'
      )
      const internal = {
        identity: null,
        attachedBackend: undefined,
        supports: () => true,
        scan: jest.fn(async () => ({
          observations: source,
          stop: async () => ({ state: 'released', failures: [] })
        })),
        connect: jest.fn(),
        destroy: jest.fn(async () => ({ state: 'released', failures: [] }))
      }
      return { source, internal }
    }
    const observe = source => {
      source.emit(
        {
          peerId: 'peer-1',
          localName: 'Sensor',
          rssi: -42,
          serviceUuids: [],
          manufacturerData: [],
          serviceData: []
        },
        1
      )
    }

    const preserved = createFindFixture()
    const defaultManager = await createPublicBleManager(preserved.internal, () => 0)
    const defaultFind = defaultManager.find({ select: 'first' })
    observe(preserved.source)
    await expect(defaultFind).resolves.toMatchObject({ id: 'peer-1' })
    // The historical `delivery: 'latest'` budget: a one-item drop-oldest window.
    expect(preserved.internal.scan).toHaveBeenCalledWith(
      expect.objectContaining({
        delivery: expect.objectContaining({ itemCapacity: 1, byteCapacity: 4 * 1024, overflowPolicy: 'drop-oldest' })
      })
    )

    const widened = createFindFixture()
    const widenedManager = await createPublicBleManager(widened.internal, () => 0)
    const widenedFind = widenedManager.find({ select: 'first', duplicates: 'all', delivery: 'balanced' })
    observe(widened.source)
    await expect(widenedFind).resolves.toMatchObject({ id: 'peer-1' })
    expect(widened.internal.scan).toHaveBeenCalledWith(
      expect.objectContaining({
        delivery: expect.objectContaining({ itemCapacity: 32, byteCapacity: 16 * 1024, overflowPolicy: 'drop-oldest' })
      })
    )
  })

  test('exposes an immutable generation-bound object graph and explicit duplicate selection', async () => {
    const publicFixture = await createPublicFixture()
    const { fixture, manager } = publicFixture
    const { connection, database } = await connectAndDiscover(fixture, manager)

    expect(database.generation).toEqual(expect.any(String))
    // Two battery services, then the heart-rate service of the duplicate-UUID world.
    expect(database.services).toHaveLength(3)
    expect(Object.isFrozen(database.services)).toBe(true)
    expect(database.servicesByUuid('180f').map(service => service.occurrence)).toEqual([0, 1])

    expect(() => database.service('180f')).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))
    const secondService = database.service('180f', { occurrence: 1 })
    expect(secondService.occurrence).toBe(1)
    expect(secondService.uuid).toBe('0000180f-0000-1000-8000-00805f9b34fb')
    expect(secondService.characteristics).toHaveLength(2)
    expect(secondService.characteristicsByUuid('2a19').map(value => value.occurrence)).toEqual([0, 1])
    expect(() => secondService.characteristic('2a19')).toThrow(expect.objectContaining({ code: 'gatt.ambiguous-path' }))

    const characteristic = secondService.characteristic('2a19', { occurrence: 1 })
    expect(characteristic.properties).toEqual({
      broadcast: false,
      read: true,
      writeWithResponse: true,
      writeWithoutResponse: true,
      authenticatedSignedWrites: false,
      notify: false,
      indicate: false,
      extendedProperties: false,
      reliableWrite: false,
      writableAuxiliaries: false,
      availability: {
        broadcast: 'unknown',
        read: 'known',
        writeWithResponse: 'known',
        writeWithoutResponse: 'known',
        authenticatedSignedWrites: 'unknown',
        notify: 'known',
        indicate: 'known',
        extendedProperties: 'unknown',
        reliableWrite: 'unknown',
        writableAuxiliaries: 'unknown'
      }
    })
    expect(Object.isFrozen(characteristic)).toBe(true)

    await expect(settle(fixture, characteristic.read())).resolves.toEqual(new Uint8Array())
    await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released' })
    await expect(characteristic.read()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })

  test('readReceipt reports the platform provenance; a read on a subscribed characteristic runs and the subscriber still receives values', async () => {
    const { fixture, manager } = await createPublicFixture()
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const idle = await settle(fixture, characteristic.readReceipt())
    expect(idle.provenance).toBe('read-response')
    expect(Object.isFrozen(idle)).toBe(true)
    const subscription = await settle(fixture, characteristic.subscribe())
    const next = subscription.values[Symbol.asyncIterator]().next()
    fixture.controller.setReadProvenance('read-or-notification')
    const fused = await settle(fixture, characteristic.readReceipt())
    expect(fused.provenance).toBe('read-or-notification')
    await expect(settle(fixture, characteristic.read())).resolves.toEqual(fused.value)
    fixture.controller.emitNotification(
      {
        serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
        serviceOccurrence: 0,
        characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: 0
      },
      new Uint8Array([0x0f])
    )
    await expect(next).resolves.toMatchObject({
      value: { kind: 'value', value: { delivery: 'notification', value: new Uint8Array([0x0f]) } }
    })
    await settle(fixture, subscription.remove())
    await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released' })
    await expect(characteristic.readReceipt()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })

  test('public notification values retain and freeze native receipt metadata', async () => {
    const receipt = {
      receivedAtUtcMs: -0.5,
      receivedAtMonotonicMs: 42.25,
      clockId: 'native-ingress-test',
      origin: 'native'
    }
    const { fixture, manager } = await createPublicFixture(undefined, undefined, receipt)
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const subscription = await settle(fixture, characteristic.subscribe())
    const next = subscription.values[Symbol.asyncIterator]().next()
    fixture.controller.emitNotification(
      {
        serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
        serviceOccurrence: 0,
        characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: 0
      },
      new Uint8Array([0x2a])
    )
    const item = await next
    expect(item.value.value.receipt).toEqual(receipt)
    expect(Object.isFrozen(item.value.value.receipt)).toBe(true)
    await settle(fixture, subscription.remove())
    await settle(fixture, connection.release())
    await settle(fixture, manager.destroy())
  })

  test('routes descriptor operations through the captured object path and invalidates the whole graph', async () => {
    const publicFixture = await createPublicFixture()
    const { fixture, manager } = publicFixture
    const { connection, database } = await connectAndDiscover(fixture, manager)
    const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
    const descriptor = characteristic.descriptor('2901')
    const changedIterator = database.changed[Symbol.asyncIterator]()
    const subscription = await settle(fixture, characteristic.subscribe())
    const subscriptionIterator = subscription.values[Symbol.asyncIterator]()

    await expect(settle(fixture, descriptor.read())).resolves.toEqual(new Uint8Array([98, 97, 116, 116, 101, 114, 121]))
    await expect(settle(fixture, descriptor.write(new Uint8Array([1, 2])))).resolves.toMatchObject({
      commitState: 'confirmed'
    })

    const preferred = await settle(fixture, characteristic.subscribe({ delivery: 'prefer-indication' }))
    const preferredValue = preferred.values[Symbol.asyncIterator]().next()
    fixture.controller.emitNotification(
      {
        serviceUuid: '0000180f-0000-1000-8000-00805f9b34fb',
        serviceOccurrence: 0,
        characteristicUuid: '00002a19-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: 0
      },
      new Uint8Array([9]),
      true
    )
    await expect(preferredValue).resolves.toMatchObject({
      value: { kind: 'value', value: { delivery: 'indication', value: new Uint8Array([9]) } }
    })
    await settle(fixture, preferred.remove())
    fixture.controller.triggerServicesChanged('deterministic-peer')
    await settle(fixture, Promise.resolve())
    await expect(changedIterator.next()).resolves.toMatchObject({
      done: false,
      value: {
        kind: 'value',
        value: { previousGeneration: database.generation, reason: 'service-changed', affectedHandleRange: null }
      }
    })
    await expect(subscriptionIterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'service-changed' }
    })
    expect(() => database.snapshot()).toThrow(expect.objectContaining({ code: 'gatt.stale-handle' }))
    await expect(descriptor.read()).rejects.toMatchObject({ code: 'gatt.stale-handle' })
    await expect(settle(fixture, connection.release())).resolves.toMatchObject({ state: 'released' })
    await expect(settle(fixture, manager.destroy())).resolves.toMatchObject({ state: 'released' })
  })
})

test('ordered subscription refuses unsupported backends without using independent streams', async () => {
  const { fixture, manager } = await createPublicFixture()
  const { connection, database } = await connectAndDiscover(fixture, manager)
  const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
  await expect(database.subscribeOrdered([{ memberId: 'battery', characteristic }])).rejects.toMatchObject({ code: 'capability.unsupported' })
  await settle(fixture, connection.release())
  await settle(fixture, manager.destroy())
})


test('ordered subscription rejects malformed and sparse members before backend admission', async () => {
  const { fixture, manager } = await createPublicFixture()
  const { connection, database } = await connectAndDiscover(fixture, manager)
  const characteristic = database.service('180f', { occurrence: 0 }).characteristic('2a19')
  for (const members of [[null], [undefined], [42], Array(1), [{ memberId: 'valid', characteristic }, ,]]) {
    await expect(database.subscribeOrdered(members)).rejects.toMatchObject({ code: 'argument.invalid' })
  }
  await settle(fixture, connection.release())
  await settle(fixture, manager.destroy())
})
