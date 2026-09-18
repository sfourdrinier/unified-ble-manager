// __tests__/ElectronIpcHardening.test.js

const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../src/electron-main')
const { ElectronRendererStreamRegistry } = require('../src/electron/renderer-stream-registry')
const { ElectronRendererBleClient } = require('../src/electron-renderer')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../src/ipc/protocol')
const { monotonicTimestamp, opaqueId, version, versionRange } = require('../src/backend-contract/primitives')
const { snapshotSerializableRecord } = require('../src/backend-contract/serializable')
const { normalizeScanQuery } = require('../src/public/scan-query')
const { snapshotScanPlan } = require('../src/backend-contract/scan-planning')

function negotiated(axis) {
  const selected = version(axis, axis === 'ipc-protocol' ? 4 : 1)
  const range = versionRange(selected, selected)
  return { axis, selected, localRange: range, remoteRange: range }
}

function emptyCapabilitySnapshot(backendGeneration) {
  return { schemaVersion: 2, backendGeneration, descriptors: [] }
}

function electronSender(fields) {
  return {
    ...fields,
    mainFrame: Object.freeze({ processId: 30, routingId: 40 })
  }
}

function mainFrameEvent(sender) {
  return {
    sender,
    frameId: sender.mainFrame.routingId,
    processId: sender.mainFrame.processId
  }
}

function createAuthority() {
  const backendGeneration = opaqueId('hardening-generation', 'backend-generation', 'hardening')
  const attachment = {
    attachmentId: opaqueId('hardening-attachment', 'attachment', 'hardening'),
    backendInstanceId: opaqueId('hardening-backend', 'backend-instance', 'hardening'),
    backendGeneration,
    adapter: {
      adapterId: opaqueId('hardening-adapter', 'adapter', 'hardening'),
      displayName: null,
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration,
        updatedAt: monotonicTimestamp(1),
        safeReason: null
      },
      adapterGeneration: opaqueId('hardening-adapter-generation', 'adapter-generation', 'hardening'),
      limitations: []
    }
  }
  const versions = {
    backendContract: negotiated('backend-contract'),
    capabilitySchema: negotiated('capability-schema'),
    eventSchema: negotiated('event-schema'),
    traceFormat: negotiated('trace-format')
  }
  return { attachment, versions }
}

function createControlledStream() {
  const values = []
  const waiters = []
  let closed = false
  function settle() {
    while (waiters.length > 0 && (closed || values.length > 0)) {
      const waiter = waiters.shift()
      waiter(values.length > 0 ? { done: false, value: values.shift() } : { done: true, value: undefined })
    }
  }
  return {
    close() {
      closed = true
      settle()
    },
    push(value) {
      values.push(value)
      settle()
    },
    [Symbol.asyncIterator]() {
      return {
        next() {
          if (values.length > 0) {
            return Promise.resolve({ done: false, value: values.shift() })
          }
          if (closed) {
            return Promise.resolve({ done: true, value: undefined })
          }
          return new Promise(resolve => waiters.push(resolve))
        }
      }
    }
  }
}

function createRouter(
  managerOverrides = {},
  maximumMessageBytes = 4096,
  publish = async () => 'delivered',
  maximumOutstandingOperations = 4,
  cancellationClock = () => 0
) {
  const authority = createAuthority()
  const manager = {
    attachedBackend: { attachment: { attachment: authority.attachment } },
    identity: { versions: authority.versions },
    capabilities: () => [],
    planScan: jest.fn(query => diagnosticPlan(query)),
    onAttachmentAdvanced: () => () => undefined,
    destroy: jest.fn(async () => ({ state: 'released', failures: [] })),
    ...managerOverrides
  }
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes,
    maximumOutstandingOperations,
    maximumRetainedBytes: 64 * 1024,
    publish,
    cancellationClock
  })
  return { ...authority, manager, router }
}

function trusted(clientId = 'hardening-client') {
  return {
    authenticatedClientId: opaqueId(clientId, 'client', `hardening:${clientId}`),
    authenticatedWindowScope: 'hardening-window',
    authenticatedSessionScope: 'hardening-session'
  }
}

function rendererLease(value) {
  return {
    leaseId: opaqueId(`hardening-renderer-lease-${value}`, 'renderer-lease', `hardening:${value}`),
    generation: opaqueId(`hardening-renderer-generation-${value}`, 'renderer-lease-generation', `hardening:${value}`)
  }
}

function diagnosticPlan(query) {
  return snapshotScanPlan({
    sourceQuery: query,
    queryDigest: query.digest,
    residualQueryDigest: query.digest,
    nativeGuarantee: 'safe-superset',
    native: { predicates: [], complete: false },
    residual: { query, predicates: [], complete: true },
    unavailable: [],
    limitations: [],
    estimatedCost: 'high'
  })
}

async function bootstrap(current, sender) {
  const response = await current.router.dispatch(sender, { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
  return response.bootstrap
}

function route(current, bootstrapValue, ordinal, command, payload, correlation = `operation-${ordinal}`) {
  const routedPayload =
    command === 'scan.start' && payload.query === undefined ? { ...payload, query: normalizeScanQuery() } : payload
  return {
    kind: 'route',
    envelope: {
      versions: bootstrapValue.versions,
      attachment: current.attachment,
      attachmentId: current.attachment.attachmentId,
      renderer: bootstrapValue.renderer,
      rendererLease: bootstrapValue.rendererLease,
      correlation: opaqueId(correlation, 'ipc-operation', `hardening:${correlation}`),
      dispatchEpoch: opaqueId(`dispatch-${ordinal}`, 'ipc-dispatch-epoch', `hardening:dispatch-${ordinal}`),
      command,
      payload: routedPayload,
      binaryPayload: null
    }
  }
}

function released() {
  return { state: 'released', failures: [] }
}

function installDestructiveCleanupResource(resources, command, handle, cleanup) {
  if (command === 'scan.stop') {
    resources.scans.set(handle, {
      scan: { stop: cleanup },
      pump: Promise.resolve(),
      cleanupRequested: false,
      retryHandle: null,
      terminalPublished: false
    })
    return { scanHandle: handle, deadline: null }
  }
  if (command === 'connection.disconnect') {
    resources.connections.set(handle, { disconnect: cleanup })
    return { connectionHandle: handle, deadline: null }
  }
  if (command === 'connection.events.unsubscribe') {
    resources.connectionEventSubscriptions.set(handle, {
      connectionHandle: 'connection-cleanup',
      iterator: { return: cleanup },
      pump: Promise.resolve(),
      cleanupRequested: false,
      cleanupResult: null,
      terminalHandled: false,
      admitted: true,
      retryHandle: null
    })
    return { connectionEventsHandle: handle, deadline: null }
  }
  resources.subscriptions.set(handle, {
    databaseHandle: 'database-cleanup',
    subscription: { remove: cleanup },
    pump: Promise.resolve(),
    cleanupRequested: false,
    retryHandle: null,
    terminalPublished: false
  })
  return { subscriptionHandle: handle, deadline: null }
}

function deferred() {
  let resolve
  let reject
  const promise = new Promise((settle, fail) => {
    resolve = settle
    reject = fail
  })
  return { promise, reject, resolve }
}

async function flushAsyncWork() {
  for (let index = 0; index < 8; index += 1) {
    await Promise.resolve()
  }
}

describe('Electron IPC hardening', () => {
  test('rejects a concurrent correlation even when the dispatch epoch differs', async () => {
    let resolveConnection
    const connectionResult = new Promise(resolve => {
      resolveConnection = resolve
    })
    const connection = {
      peerId: 'peer-collision',
      disconnect: jest.fn(async () => released())
    }
    const current = createRouter({ connect: jest.fn(async () => connectionResult) })
    const sender = trusted()
    const bootstrapValue = await bootstrap(current, sender)
    const first = current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-collision' }, 'same-correlation')
    )
    await flushAsyncWork()
    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'connection.connect', { peerId: 'peer-collision' }, 'same-correlation')
      )
    ).rejects.toMatchObject({
      normalized: { code: 'protocol.violation', operation: 'electron-main-router.correlation-in-flight' }
    })
    resolveConnection(connection)
    await expect(first).resolves.toMatchObject({ kind: 'route', payload: { peerId: 'peer-collision' } })
    await current.router.destroy()
  })

  test('routes cancellation when normal operations have exhausted the outstanding-operation quota', async () => {
    let resolveConnection
    const connectionResult = new Promise(resolve => {
      resolveConnection = resolve
    })
    let operationSignal = null
    const connection = {
      peerId: 'peer-cancellation-capacity',
      disconnect: jest.fn(async () => released())
    }
    const current = createRouter(
      {
        connect: jest.fn(async (_peerId, options) => {
          operationSignal = options.signal
          return connectionResult
        })
      },
      4096,
      undefined,
      1
    )
    const sender = trusted('cancellation-capacity-client')
    const bootstrapValue = await bootstrap(current, sender)
    const pending = current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-cancellation-capacity' })
    )
    await flushAsyncWork()

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'operation.cancel', { targetCorrelation: 'operation-1' })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-requested' } })
    expect(operationSignal.aborted).toBe(true)

    resolveConnection(connection)
    await expect(pending).rejects.toMatchObject({
      normalized: { code: 'operation.aborted', operation: 'electron-main-router.connection.connect' }
    })
    expect(connection.disconnect).toHaveBeenCalledTimes(1)
    await current.router.destroy()
  })

  test('consumes a lease-scoped cancellation received before its operation route is registered', async () => {
    const connect = jest.fn(async () => ({ peerId: 'peer-pre-cancel', disconnect: jest.fn(async () => released()) }))
    const current = createRouter({ connect })
    const sender = trusted('pre-cancel-client')
    const bootstrapValue = await bootstrap(current, sender)

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'operation.cancel', { targetCorrelation: 'pre-cancelled-operation' })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-pending' } })
    await expect(
      current.router.dispatch(
        sender,
        route(
          current,
          bootstrapValue,
          2,
          'connection.connect',
          { peerId: 'peer-pre-cancel' },
          'pre-cancelled-operation'
        )
      )
    ).rejects.toMatchObject({
      normalized: { code: 'operation.aborted', operation: 'electron-main-router.connection.connect' }
    })
    expect(connect).not.toHaveBeenCalled()
    expect(
      current.router.resources.get(String(bootstrapValue.rendererLease.leaseId)).preCancelledOperations
    ).toHaveProperty('size', 0)
    await current.router.destroy()
  })

  test('bounds lease-scoped pre-cancellation tombstones and rejects a false acknowledgement when full', async () => {
    const current = createRouter({}, 4096, async () => 'delivered', 1)
    const sender = trusted('pre-cancel-capacity-client')
    const bootstrapValue = await bootstrap(current, sender)

    await current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'operation.cancel', { targetCorrelation: 'pending-cancel-1' })
    )
    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'operation.cancel', { targetCorrelation: 'pending-cancel-2' })
      )
    ).rejects.toMatchObject({
      normalized: { code: 'stream.quota', operation: 'electron-main-router.pre-cancellation-capacity' }
    })
    await current.router.destroy()
  })

  test('does not turn a late cancellation for a settled operation into a pre-cancellation tombstone', async () => {
    const connection = { peerId: 'peer-settled-cancellation', disconnect: jest.fn(async () => released()) }
    const current = createRouter({ connect: jest.fn(async () => connection) })
    const sender = trusted('settled-cancellation-client')
    const bootstrapValue = await bootstrap(current, sender)

    await expect(
      current.router.dispatch(
        sender,
        route(
          current,
          bootstrapValue,
          1,
          'connection.connect',
          { peerId: 'peer-settled-cancellation' },
          'settled-operation'
        )
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { peerId: 'peer-settled-cancellation' } })

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'operation.cancel', { targetCorrelation: 'settled-operation' })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'already-terminal' } })
    await current.router.destroy()
  })

  test('expires lease-scoped pre-cancellation tombstones so their bounded capacity remains reusable', async () => {
    let now = 0
    const current = createRouter(
      {},
      4096,
      async () => 'delivered',
      1,
      () => now
    )
    const sender = trusted('pre-cancellation-expiry-client')
    const bootstrapValue = await bootstrap(current, sender)

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'operation.cancel', { targetCorrelation: 'expired-pending-cancel' })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-pending' } })

    now = 30_001
    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'operation.cancel', { targetCorrelation: 'replacement-pending-cancel' })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-pending' } })
    const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))
    expect(resources.preCancelledOperations).toHaveProperty('size', 1)
    expect(resources.preCancelledOperations.has('replacement-pending-cancel')).toBe(true)
    await current.router.destroy()
  })

  test.each(['scan.stop', 'connection.disconnect', 'connection.events.unsubscribe', 'gatt.unsubscribe'])(
    '%s returns its irreversible cleanup receipt when cancellation or deadline arrives after cleanup starts',
    async command => {
      let now = 0
      const current = createRouter({ monotonicNow: () => now })
      const sender = trusted(`destructive-cleanup-${command}`)
      const bootstrapValue = await bootstrap(current, sender)
      const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))

      for (const terminalCondition of ['cancelled', 'timed-out']) {
        const cleanupStarted = deferred()
        const cleanupResult = deferred()
        const handle = `${command}-${terminalCondition}`
        const cleanup = jest.fn(async () => {
          cleanupStarted.resolve()
          return cleanupResult.promise
        })
        const payload = installDestructiveCleanupResource(resources, command, handle, cleanup)
        if (terminalCondition === 'timed-out') {
          payload.budgetMs = 10
        }
        const correlation = `${command}-${terminalCondition}-operation`
        const operation = current.router.dispatch(
          sender,
          route(current, bootstrapValue, 10, command, payload, correlation)
        )
        await cleanupStarted.promise
        if (terminalCondition === 'cancelled') {
          await expect(
            current.router.dispatch(
              sender,
              route(current, bootstrapValue, 11, 'operation.cancel', { targetCorrelation: correlation })
            )
          ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-requested' } })
        } else {
          now = 20
        }
        cleanupResult.resolve(released())
        await expect(operation).resolves.toMatchObject({ kind: 'route', payload: { state: 'released', failures: [] } })
        expect(cleanup).toHaveBeenCalledTimes(1)
        now = 0
      }
      await current.router.destroy()
    }
  )

  // PR210-27: a write that finished has committed at the peripheral. Turning
  // its receipt into aborted/timed-out `caller-decides` invites the caller to
  // send the same write a second time, so the completed result is reported.
  test.each(['gatt.write', 'gatt.descriptor.write'])(
    '%s returns its completed receipt when cancellation or deadline arrives after dispatch',
    async command => {
      let now = 0
      const current = createRouter({ monotonicNow: () => now })
      const sender = trusted(`completed-write-${command}`)
      const bootstrapValue = await bootstrap(current, sender)
      const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))

      for (const terminalCondition of ['cancelled', 'timed-out']) {
        const writeStarted = deferred()
        const writeResult = deferred()
        const write = jest.fn(async () => {
          writeStarted.resolve()
          return writeResult.promise
        })
        const databaseHandle = `database-${terminalCondition}`
        resources.databases.set(databaseHandle, {
          database: { write, writeDescriptor: write },
          characteristics: new Map([['characteristic-1', { characteristicUuid: 'characteristic-1' }]]),
          descriptors: new Map([['descriptor-1', { descriptorUuid: 'descriptor-1' }]])
        })
        const payload =
          command === 'gatt.write'
            ? { databaseHandle, characteristicHandle: 'characteristic-1', mode: 'with-response', deadline: null }
            : { databaseHandle, descriptorHandle: 'descriptor-1', deadline: null }
        if (terminalCondition === 'timed-out') {
          payload.budgetMs = 10
        }
        const correlation = `${command}-${terminalCondition}-operation`
        const request = route(current, bootstrapValue, 20, command, payload, correlation)
        request.envelope.binaryPayload = new Uint8Array([7, 8, 9])
        const operation = current.router.dispatch(sender, request)
        await writeStarted.promise
        if (terminalCondition === 'cancelled') {
          await expect(
            current.router.dispatch(
              sender,
              route(current, bootstrapValue, 21, 'operation.cancel', { targetCorrelation: correlation })
            )
          ).resolves.toMatchObject({ kind: 'route', payload: { state: 'cancellation-requested' } })
        } else {
          now = 20
        }
        writeResult.resolve({
          terminal: { correlation, outcome: 'succeeded', cause: null },
          commitState: 'confirmed'
        })
        await expect(operation).resolves.toMatchObject({
          kind: 'route',
          payload: { terminal: { outcome: 'succeeded' }, commitState: 'confirmed', bytesSubmitted: 3 }
        })
        expect(write).toHaveBeenCalledTimes(1)
        now = 0
      }
      await current.router.destroy()
    }
  )

  test('a write refused before dispatch stays caller-decides because nothing reached the radio', async () => {
    let now = 20
    const current = createRouter({ monotonicNow: () => now })
    const sender = trusted('pre-dispatch-write')
    const bootstrapValue = await bootstrap(current, sender)
    const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))
    const write = jest.fn()
    resources.databases.set('database-expired', {
      database: { write },
      characteristics: new Map([['characteristic-1', { characteristicUuid: 'characteristic-1' }]]),
      descriptors: new Map()
    })
    const request = route(current, bootstrapValue, 30, 'gatt.write', {
      databaseHandle: 'database-expired',
      characteristicHandle: 'characteristic-1',
      mode: 'with-response',
      budgetMs: 0
    })
    request.envelope.binaryPayload = new Uint8Array([1])

    await expect(current.router.dispatch(sender, request)).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out', retryability: 'caller-decides' }
    })
    expect(write).not.toHaveBeenCalled()
    now = 0
    await current.router.destroy()
  })

  test('compensates a late deadline during lifecycle readiness by detaching its newly admitted iterator', async () => {
    let nextCalls = 0
    let resolveNext
    const iterator = {
      next: jest.fn(() => {
        nextCalls += 1
        return new Promise(resolve => {
          resolveNext = resolve
        })
      }),
      return: jest.fn(async () => {
        resolveNext({ done: true, value: undefined })
        return { done: true, value: undefined }
      })
    }
    const current = createRouter({ monotonicNow: () => (nextCalls === 0 ? 0 : 20) })
    const sender = trusted('lifecycle-ready-deadline-client')
    const bootstrapValue = await bootstrap(current, sender)
    const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))
    resources.connectionEventSubscriptions.set('connection-events-ready-deadline', {
      connectionHandle: 'connection-ready-deadline',
      iterator,
      pump: Promise.resolve(),
      cleanupRequested: false,
      cleanupResult: null,
      terminalHandled: false,
      admitted: false,
      retryHandle: null
    })

    const readiness = current.router.dispatch(
      sender,
      route(
        current,
        bootstrapValue,
        1,
        'connection.events.ready',
        { connectionEventsHandle: 'connection-events-ready-deadline', budgetMs: 10 },
        'ready-deadline'
      )
    )
    await flushAsyncWork()
    await expect(readiness).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out', operation: 'electron-main-router.connection.events.ready' }
    })
    expect(nextCalls).toBe(1)
    expect(iterator.return).toHaveBeenCalledTimes(1)
    expect(resources.connectionEventSubscriptions).toHaveProperty('size', 0)
    await current.router.destroy()
  })

  test('counts renderer lease identity in outbound event backlog admission', async () => {
    let publish
    const lease = rendererLease('event-byte-accounting')
    const sender = electronSender({
      trusted: trusted('event-byte-accounting'),
      send: jest.fn(),
      once: jest.fn(),
      on: jest.fn(),
      removeListener: jest.fn()
    })
    const router = {
      setEventPublisher(listener) {
        publish = listener
      },
      validateRequest: jest.fn(),
      dispatch: jest.fn(async authenticated => ({
        kind: 'bootstrap',
        bootstrap: {
          renderer: {
            clientId: authenticated.authenticatedClientId,
            windowScope: authenticated.authenticatedWindowScope,
            sessionScope: authenticated.authenticatedSessionScope
          },
          rendererLease: lease
        }
      })),
      releaseRenderer: jest.fn(async () => released()),
      terminateStream: jest.fn(async () => undefined),
      destroy: jest.fn(async () => released())
    }
    const port = {
      handle(_channel, handler) {
        this.handler = handler
      },
      removeHandler: jest.fn()
    }
    const binding = new ElectronMainBleBinding({ router, port, authenticate: event => event.sender.trusted })
    binding.install()
    await port.handler(mainFrameEvent(sender), { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })

    const eventBase = {
      rendererLease: lease,
      eventId: 'event-byte-accounting',
      streamId: 'stream-byte-accounting',
      item: { kind: 'value', payload: '' }
    }
    const unscopedBaseBytes = snapshotSerializableRecord({
      eventId: eventBase.eventId,
      streamId: eventBase.streamId,
      item: eventBase.item
    }).byteLength
    const event = {
      ...eventBase,
      item: { kind: 'value', payload: 'x'.repeat(512 * 1024 - unscopedBaseBytes) }
    }
    const unscopedBytes = snapshotSerializableRecord({
      eventId: event.eventId,
      streamId: event.streamId,
      item: event.item
    }).byteLength
    const scopedBytes = snapshotSerializableRecord({
      rendererLease: { leaseId: String(lease.leaseId), generation: String(lease.generation) },
      eventId: event.eventId,
      streamId: event.streamId,
      item: event.item
    }).byteLength

    expect(unscopedBytes).toBeLessThanOrEqual(512 * 1024)
    expect(scopedBytes).toBeGreaterThan(512 * 1024)
    await expect(publish(String(lease.leaseId), event)).resolves.toBe('terminalized')
    expectConsoleError('[ElectronMainBleBinding] Renderer event budget exhausted:', {
      rendererLeaseId: String(lease.leaseId),
      streamId: event.streamId,
      terminal: false
    })
    expect(sender.send).not.toHaveBeenCalled()
    expect(router.terminateStream).toHaveBeenCalledWith(lease, event.streamId, 'renderer-backpressure')
    await binding.destroy()
  })

  test('includes renderer lease identity when enforcing the stream event message limit', async () => {
    const lease = rendererLease('stream-byte-accounting')
    const stream = createControlledStream()
    const stop = jest.fn(async () => {
      stream.close()
      return released()
    })
    const resources = { scans: new Map(), subscriptions: new Map() }
    const events = []
    const maximumMessageBytes = 4096
    let nextEvent = 1
    const streamId = 'scan-byte-accounting'
    const eventId = 'event-1'
    const emptyEventItem = {
      kind: 'value',
      value: { value: new Uint8Array(), delivery: 'notification' }
    }
    const unscopedBaseBytes = snapshotSerializableRecord({ eventId, streamId, item: emptyEventItem }).byteLength
    const item = {
      kind: 'value',
      value: { value: new Uint8Array(maximumMessageBytes - unscopedBaseBytes), delivery: 'notification' }
    }
    const unscopedBytes = snapshotSerializableRecord({ eventId, streamId, item }).byteLength
    const scopedBytes = snapshotSerializableRecord({
      rendererLease: { leaseId: String(lease.leaseId), generation: String(lease.generation) },
      eventId,
      streamId,
      item
    }).byteLength
    const registry = new ElectronRendererStreamRegistry({
      maximumMessageBytes,
      publish: async (_rendererLeaseId, event) => {
        events.push(event)
        return 'delivered'
      },
      createEvent: (rendererLease, nextStreamId, nextItem) => ({
        rendererLease,
        eventId: `event-${nextEvent++}`,
        streamId: nextStreamId,
        item: nextItem
      })
    })

    expect(unscopedBytes).toBeLessThanOrEqual(maximumMessageBytes)
    expect(scopedBytes).toBeGreaterThan(maximumMessageBytes)
    registry.registerScan(resources, lease, streamId, { observations: stream, stop })
    stream.push(item)
    await flushAsyncWork()

    expect(stop).toHaveBeenCalledTimes(1)
    expect(events).toEqual([
      expect.objectContaining({ item: expect.objectContaining({ kind: 'terminal', reason: 'ipc-message-too-large' }) })
    ])
    expectConsoleError('[ElectronRendererStreamRegistry] Stream item exceeded the configured IPC message limit:', {
      streamId
    })
  })

  test('forwards complete advertisements with fractional monotonic timestamps and owned binary fields', async () => {
    const lease = rendererLease('advertisement-stream')
    const stream = createControlledStream()
    const stop = jest.fn(async () => {
      stream.close()
      return released()
    })
    const resources = { scans: new Map(), subscriptions: new Map() }
    const events = []
    let nextEvent = 1
    const registry = new ElectronRendererStreamRegistry({
      maximumMessageBytes: 16 * 1024,
      publish: async (_rendererLeaseId, event) => {
        events.push(event)
        return 'delivered'
      },
      createEvent: (rendererLease, streamId, item) => ({
        rendererLease,
        eventId: `advertisement-event-${nextEvent++}`,
        streamId,
        item
      })
    })
    const unavailable = {
      state: 'unavailable',
      reason: 'CoreBluetooth boundary did not provide this advertisement field',
      provenance: 'not-provided'
    }
    const observation = {
      device: {
        id: 'peer-advertisement',
        backendInstanceId: 'backend-advertisement',
        scope: 'backend',
        stableAcrossRestarts: false,
        address: null
      },
      provenance: 'platform-derived',
      sourceTimestamp: {
        state: 'present',
        value: { monotonicMs: 12.25, origin: 'platform' },
        provenance: 'observed'
      },
      receivedAtMonotonicMs: 42.75,
      ingressOrdinal: 1,
      scanSessionId: 'scan-advertisement',
      localName: { state: 'present', value: 'Test sensor', provenance: 'observed' },
      rssi: { state: 'present', value: -42, provenance: 'observed' },
      txPower: unavailable,
      connectable: { state: 'present', value: true, provenance: 'observed' },
      appearance: unavailable,
      serviceUuids: { state: 'present', value: ['180d'], provenance: 'observed' },
      solicitedServiceUuids: unavailable,
      overflowServiceUuids: unavailable,
      serviceData: {
        state: 'present',
        value: [{ serviceUuid: '180d', value: new Uint8Array([1, 2]) }],
        provenance: 'observed'
      },
      manufacturerData: {
        state: 'present',
        value: [{ companyIdentifier: 0x006b, value: new Uint8Array([3, 4]) }],
        provenance: 'observed'
      },
      rawRecord: { state: 'present', value: new Uint8Array([5, 6]), provenance: 'observed' },
      scanResponseRecord: unavailable
    }

    registry.registerScan(resources, lease, 'scan-advertisement', { observations: stream, stop })
    stream.push({ kind: 'value', value: observation })
    await flushAsyncWork()

    expect(events).toHaveLength(1)
    expect(events[0]).toMatchObject({
      item: {
        kind: 'value',
        value: {
          receivedAtMonotonicMs: 42.75,
          manufacturerData: {
            state: 'present',
            value: [{ companyIdentifier: 0x006b, value: new Uint8Array([3, 4]) }]
          },
          rawRecord: { state: 'present', value: new Uint8Array([5, 6]) }
        }
      }
    })

    stream.push({
      kind: 'terminal',
      reason: 'completed',
      droppedItems: 0,
      droppedBytes: 0,
      replacedItems: 0
    })
    await flushAsyncWork()
    expect(stop).toHaveBeenCalledTimes(1)
  })

  test('rejects oversized responses and removes newly allocated discovery handles', async () => {
    const characteristics = Array.from({ length: 64 }, (_, index) => ({
      path: {
        serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
        serviceOccurrence: index,
        characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: index
      }
    }))
    const database = { snapshot: jest.fn(async () => ({ characteristics })) }
    const connection = {
      peerId: 'peer-large',
      discover: jest.fn(async () => database),
      disconnect: jest.fn(async () => released())
    }
    const current = createRouter({ connect: jest.fn(async () => connection) }, 1024)
    const sender = trusted('large-client')
    const bootstrapValue = await bootstrap(current, sender)
    const connected = await current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-large' })
    )
    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 2, 'gatt.discover', { connectionHandle: connected.payload.handle })
      )
    ).rejects.toMatchObject({
      normalized: { code: 'bytes.too-large', operation: 'electron-main-router.response-size' }
    })
    expect(current.router.resources.get(String(bootstrapValue.rendererLease.leaseId)).databases.size).toBe(0)
    await current.router.destroy()
  })

  test('cleans natural source terminals and bare exhaustion exactly once', async () => {
    const events = []
    const terminalStream = createControlledStream()
    const terminalStop = jest.fn(async () => {
      terminalStream.close()
      return released()
    })
    const bareStream = createControlledStream()
    const bareStop = jest.fn(async () => {
      bareStream.close()
      return released()
    })
    const scans = [
      { observations: terminalStream, stop: terminalStop },
      { observations: bareStream, stop: bareStop }
    ]
    const current = createRouter({ scan: jest.fn(async () => scans.shift()) }, 4096, async (_clientId, event) => {
      events.push(event)
      return 'delivered'
    })
    const sender = trusted('streams-client')
    const bootstrapValue = await bootstrap(current, sender)
    await current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'scan.start', {
        serviceUuids: [],
        manufacturerData: [],
        localNamePrefix: null,
        deadline: null
      })
    )
    terminalStream.push({
      kind: 'terminal',
      reason: 'closed',
      droppedItems: 0,
      droppedBytes: 0,
      replacedItems: 0
    })
    await flushAsyncWork()
    expect(terminalStop).toHaveBeenCalledTimes(1)
    expect(events.filter(event => event.item.kind === 'terminal')).toHaveLength(1)

    await current.router.dispatch(
      sender,
      route(current, bootstrapValue, 2, 'scan.start', {
        serviceUuids: [],
        manufacturerData: [],
        localNamePrefix: null,
        deadline: null
      })
    )
    bareStream.close()
    await flushAsyncWork()
    expect(bareStop).toHaveBeenCalledTimes(1)
    expect(events.filter(event => event.item.kind === 'terminal')).toHaveLength(2)
    expect(events[1].item.reason).toBe('source-failed')
    expect(current.router.resources.get(String(bootstrapValue.rendererLease.leaseId)).scans.size).toBe(0)
    expectConsoleError('[ElectronRendererStreamRegistry] Stream ended without a terminal item:', {
      streamId: 'scan-2'
    })
    await current.router.destroy()
  })

  test('rejects direct acknowledgements and oversized acknowledgement controls', async () => {
    const current = createRouter({}, 128)
    const sender = trusted('ack-client')
    const bootstrapValue = await bootstrap(current, sender)
    await expect(
      current.router.dispatch(sender, {
        kind: 'event.ack',
        rendererLease: bootstrapValue.rendererLease,
        eventId: 'event-direct'
      })
    ).rejects.toMatchObject({
      normalized: { code: 'protocol.violation', operation: 'electron-main-router.event-ack-binding-required' }
    })
    expect(() =>
      current.router.validateRequest({
        kind: 'event.ack',
        rendererLease: bootstrapValue.rendererLease,
        eventId: 'x'.repeat(1024)
      })
    ).toThrow()
    await current.router.destroy()
  })

  test('binds acknowledgements to the exact authenticated WebContents', async () => {
    let publish
    const lease = rendererLease('bound-client')
    const router = {
      setEventPublisher(listener) {
        publish = listener
      },
      validateRequest: jest.fn(),
      dispatch: jest.fn(async sender => ({
        kind: 'bootstrap',
        bootstrap: {
          renderer: {
            clientId: sender.authenticatedClientId,
            windowScope: sender.authenticatedWindowScope,
            sessionScope: sender.authenticatedSessionScope
          },
          rendererLease: lease
        }
      })),
      releaseRenderer: jest.fn(async () => released()),
      terminateStream: jest.fn(async () => undefined),
      destroy: jest.fn(async () => released())
    }
    const port = {
      handle(_channel, handler) {
        this.handler = handler
      },
      removeHandler: jest.fn()
    }
    const senderA = electronSender({
      trusted: trusted('bound-client'),
      sent: [],
      send(_channel, event) {
        this.sent.push(event)
      },
      once: jest.fn(),
      on: jest.fn(),
      removeListener: jest.fn()
    })
    const senderB = electronSender({
      trusted: senderA.trusted,
      sent: [],
      send(_channel, event) {
        this.sent.push(event)
      },
      once: jest.fn(),
      on: jest.fn(),
      removeListener: jest.fn()
    })
    const binding = new ElectronMainBleBinding({ router, port, authenticate: event => event.sender.trusted })
    binding.install()
    await port.handler(mainFrameEvent(senderA), { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
    await publish(String(lease.leaseId), {
      rendererLease: lease,
      eventId: 'event-bound',
      streamId: 'scan-bound',
      item: { kind: 'value' }
    })
    await expect(
      port.handler(mainFrameEvent(senderB), { kind: 'event.ack', rendererLease: lease, eventId: 'event-bound' })
    ).resolves.toMatchObject({ kind: 'failure', error: { code: 'ownership.denied' } })
    await expect(
      port.handler(mainFrameEvent(senderB), { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
    ).resolves.toMatchObject({
      kind: 'failure',
      error: { code: 'ownership.denied' }
    })
    await expect(
      port.handler(mainFrameEvent(senderA), { kind: 'event.ack', rendererLease: lease, eventId: 'event-bound' })
    ).resolves.toEqual({ kind: 'event.ack' })
    await expect(
      port.handler(mainFrameEvent(senderA), { kind: 'event.ack', rendererLease: lease, eventId: 'event-bound' })
    ).resolves.toEqual({ kind: 'event.ack' })
    await binding.destroy()
  })

  test('aggregates router and manager destroy rejections into cleanup records', async () => {
    const current = createRouter({
      destroy: jest.fn(async () => {
        throw new Error('manager destroy rejected')
      })
    })
    await expect(current.router.destroy()).resolves.toMatchObject({
      state: 'release-failed',
      failures: [{ resourceKind: 'manager' }]
    })
    expectConsoleErrorMatching(
      '[ElectronMainBleRouter] Manager cleanup rejected during router destroy:',
      expect.objectContaining({ message: 'manager destroy rejected' })
    )

    const router = {
      setEventPublisher: jest.fn(),
      validateRequest: jest.fn(),
      dispatch: jest.fn(),
      releaseRenderer: jest.fn(),
      terminateStream: jest.fn(),
      destroy: jest.fn(async () => {
        throw new Error('router destroy rejected')
      })
    }
    const port = { handle: jest.fn(), removeHandler: jest.fn() }
    const binding = new ElectronMainBleBinding({ router, port, authenticate: jest.fn() })
    await expect(binding.destroy()).resolves.toMatchObject({
      state: 'release-failed',
      failures: [{ resourceKind: 'electron-router' }]
    })
    expectConsoleErrorMatching(
      '[ElectronMainBleBinding] Router destroy rejected:',
      expect.objectContaining({ message: 'router destroy rejected' })
    )
  })

  test('retries ambiguous renderer acknowledgements against an idempotent main ledger', async () => {
    jest.useFakeTimers()
    try {
      const listeners = []
      const bootstrapValue = {
        attachment: createAuthority().attachment,
        attachmentId: createAuthority().attachment.attachmentId,
        versions: { ...createAuthority().versions, ipcProtocol: negotiated('ipc-protocol') },
        capabilities: emptyCapabilitySnapshot(createAuthority().attachment.backendGeneration),
        renderer: {
          clientId: opaqueId('ack-retry-client', 'client', 'hardening:ack-retry'),
          windowScope: 'ack-retry-window',
          sessionScope: 'ack-retry-session'
        },
        rendererLease: rendererLease('ack-retry-client')
      }
      const acknowledge = jest
        .fn()
        .mockRejectedValueOnce(new Error('ack response lost'))
        .mockResolvedValueOnce({ kind: 'event.ack' })
      const transport = {
        invoke: jest.fn(async request =>
          request.kind === 'bootstrap'
            ? { kind: 'bootstrap', bootstrap: bootstrapValue }
            : { kind: 'release', cleanup: released() }
        ),
        acknowledge,
        subscribe(listener) {
          listeners.push(listener)
          return () => listeners.splice(listeners.indexOf(listener), 1)
        }
      }
      const client = new ElectronRendererBleClient(transport)
      await client.initialize()
      listeners[0]({
        rendererLease: bootstrapValue.rendererLease,
        eventId: 'event-retry',
        streamId: 'scan-retry',
        item: { kind: 'value' }
      })
      await flushAsyncWork()
      expect(acknowledge).toHaveBeenCalledTimes(1)
      expect(acknowledge).toHaveBeenNthCalledWith(1, bootstrapValue.rendererLease, 'event-retry')
      await jest.advanceTimersByTimeAsync(100)
      expect(acknowledge).toHaveBeenCalledTimes(2)
      expect(acknowledge).toHaveBeenNthCalledWith(2, bootstrapValue.rendererLease, 'event-retry')
      await client.destroy()
      expectConsoleErrorMatching(
        '[ElectronRendererBleClient] Event acknowledgement failed; retry scheduled:',
        expect.objectContaining({
          eventId: 'event-retry',
          error: expect.objectContaining({ message: 'ack response lost' })
        })
      )
    } finally {
      jest.useRealTimers()
    }
  })

  test('terminates the renderer event stream without retrying when acknowledgement reports a lost renderer lease', async () => {
    jest.useFakeTimers()
    try {
      const listeners = []
      const bootstrapValue = {
        attachment: createAuthority().attachment,
        attachmentId: createAuthority().attachment.attachmentId,
        versions: { ...createAuthority().versions, ipcProtocol: negotiated('ipc-protocol') },
        capabilities: emptyCapabilitySnapshot(createAuthority().attachment.backendGeneration),
        renderer: {
          clientId: opaqueId('ack-lease-lost-client', 'client', 'hardening:ack-lease-lost'),
          windowScope: 'ack-lease-lost-window',
          sessionScope: 'ack-lease-lost-session'
        },
        rendererLease: rendererLease('ack-lease-lost-client')
      }
      const rendererRegistrationFailure = {
        code: 'ownership.denied',
        domain: 'ipc',
        operation: 'electron-main-arbiter.renderer-registration',
        platform: null,
        retryability: 'never'
      }
      const acknowledge = jest.fn(async () => ({ kind: 'failure', error: rendererRegistrationFailure }))
      const transport = {
        invoke: jest.fn(async request =>
          request.kind === 'bootstrap'
            ? { kind: 'bootstrap', bootstrap: bootstrapValue }
            : { kind: 'release', cleanup: released() }
        ),
        acknowledge,
        subscribe(listener) {
          listeners.push(listener)
          return () => listeners.splice(listeners.indexOf(listener), 1)
        }
      }
      const client = new ElectronRendererBleClient(transport)
      await client.initialize()

      listeners[0]({
        rendererLease: bootstrapValue.rendererLease,
        eventId: 'event-lease-lost',
        streamId: 'scan-lease-lost',
        item: { kind: 'value' }
      })
      await flushAsyncWork()

      expect(acknowledge).toHaveBeenCalledTimes(1)
      await jest.advanceTimersByTimeAsync(100)
      expect(acknowledge).toHaveBeenCalledTimes(1)
      const iterator = client.events[Symbol.asyncIterator]()
      await expect(iterator.next()).resolves.toMatchObject({
        done: false,
        value: { kind: 'terminal', reason: 'owner-released' }
      })
      await expect(client.destroy()).resolves.toEqual(released())
      expect(transport.invoke).toHaveBeenCalledTimes(1)
      expect(listeners).toEqual([])
      expectConsoleError(
        '[ElectronRendererBleClient] Event acknowledgement failed permanently; terminating event delivery:',
        {
          error: rendererRegistrationFailure
        }
      )
    } finally {
      jest.useRealTimers()
    }
  })

  test('terminates safely and leaves release available for other permanent acknowledgement failures', async () => {
    const listeners = []
    const bootstrapValue = {
      attachment: createAuthority().attachment,
      attachmentId: createAuthority().attachment.attachmentId,
      versions: { ...createAuthority().versions, ipcProtocol: negotiated('ipc-protocol') },
      capabilities: emptyCapabilitySnapshot(createAuthority().attachment.backendGeneration),
      renderer: {
        clientId: opaqueId('ack-permanent-failure-client', 'client', 'hardening:ack-permanent-failure'),
        windowScope: 'ack-permanent-failure-window',
        sessionScope: 'ack-permanent-failure-session'
      },
      rendererLease: rendererLease('ack-permanent-failure-client')
    }
    const transport = {
      invoke: jest.fn(async request =>
        request.kind === 'bootstrap'
          ? { kind: 'bootstrap', bootstrap: bootstrapValue }
          : { kind: 'release', cleanup: released() }
      ),
      acknowledge: jest.fn(async () => ({
        kind: 'failure',
        error: {
          code: 'protocol.violation',
          domain: 'ipc',
          operation: 'electron-main-binding.event-ack-replay',
          platform: null,
          retryability: 'never'
        }
      })),
      subscribe(listener) {
        listeners.push(listener)
        return () => listeners.splice(listeners.indexOf(listener), 1)
      }
    }
    const client = new ElectronRendererBleClient(transport)
    await client.initialize()

    listeners[0]({
      rendererLease: bootstrapValue.rendererLease,
      eventId: 'event-permanent-failure',
      streamId: 'scan-permanent-failure',
      item: { kind: 'value' }
    })
    await flushAsyncWork()

    const iterator = client.events[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      done: false,
      value: { kind: 'terminal', reason: 'source-failed' }
    })
    await expect(client.destroy()).resolves.toEqual(released())
    expect(transport.invoke).toHaveBeenNthCalledWith(2, {
      kind: 'release',
      rendererLease: bootstrapValue.rendererLease
    })
    expect(listeners).toEqual([])
    expectConsoleError(
      '[ElectronRendererBleClient] Event acknowledgement failed permanently; terminating event delivery:',
      {
        error: {
          code: 'protocol.violation',
          domain: 'ipc',
          operation: 'electron-main-binding.event-ack-replay',
          platform: null,
          retryability: 'never'
        }
      }
    )
  })

  test('retries destroyed WebContents cleanup until ownership is released', async () => {
    jest.useFakeTimers()
    try {
      let destroyedListener
      const lease = rendererLease('destroyed-retry-client')
      const sender = electronSender({
        trusted: trusted('destroyed-retry-client'),
        send: jest.fn(),
        once(_event, listener) {
          destroyedListener = listener
        },
        on: jest.fn(),
        removeListener: jest.fn()
      })
      const router = {
        setEventPublisher: jest.fn(),
        validateRequest: jest.fn(),
        dispatch: jest.fn(async authenticated => ({
          kind: 'bootstrap',
          bootstrap: {
            renderer: {
              clientId: authenticated.authenticatedClientId,
              windowScope: authenticated.authenticatedWindowScope,
              sessionScope: authenticated.authenticatedSessionScope
            },
            rendererLease: lease
          }
        })),
        releaseRenderer: jest
          .fn()
          .mockResolvedValueOnce({
            state: 'release-failed',
            failures: [{ resourceKind: 'scan', error: { code: 'platform.failure' } }]
          })
          .mockResolvedValueOnce(released()),
        terminateStream: jest.fn(),
        destroy: jest.fn(async () => released())
      }
      const port = {
        handle(_channel, handler) {
          this.handler = handler
        },
        removeHandler: jest.fn()
      }
      const binding = new ElectronMainBleBinding({ router, port, authenticate: event => event.sender.trusted })
      binding.install()
      await port.handler(mainFrameEvent(sender), { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
      destroyedListener()
      await flushAsyncWork()
      expect(router.releaseRenderer).toHaveBeenCalledTimes(1)
      await jest.advanceTimersByTimeAsync(100)
      expect(router.releaseRenderer).toHaveBeenCalledTimes(2)
      await binding.destroy()
      expectConsoleErrorMatching(
        '[ElectronMainBleBinding] Renderer lifetime cleanup reported failures:',
        expect.objectContaining({ rendererLeaseId: String(lease.leaseId) })
      )
    } finally {
      jest.useRealTimers()
    }
  })

  test('does not issue an unscoped release when a bootstrap response is lost', async () => {
    const listeners = []
    const transport = {
      invoke: jest.fn().mockRejectedValueOnce(new Error('bootstrap response lost')),
      acknowledge: jest.fn(),
      subscribe(listener) {
        listeners.push(listener)
        return () => listeners.splice(listeners.indexOf(listener), 1)
      }
    }
    const client = new ElectronRendererBleClient(transport)
    const initialization = client.initialize()
    const destruction = client.destroy()
    await expect(initialization).rejects.toThrow('bootstrap response lost')
    await expect(destruction).resolves.toEqual(released())
    expect(transport.invoke).toHaveBeenCalledTimes(1)
    expect(listeners).toEqual([])
    expectConsoleErrorMatching(
      '[ElectronRendererBleClient] Initialization failed during destroy; releasing main ownership:',
      expect.objectContaining({ message: 'bootstrap response lost' })
    )
  })

  test('retries native cleanup after a natural stream terminal without publishing a second terminal', async () => {
    jest.useFakeTimers()
    try {
      const stream = createControlledStream()
      const stop = jest
        .fn()
        .mockResolvedValueOnce({
          state: 'release-failed',
          failures: [{ resourceKind: 'scan', error: { code: 'platform.failure' } }]
        })
        .mockImplementationOnce(async () => {
          stream.close()
          return released()
        })
      const events = []
      const current = createRouter(
        { scan: jest.fn(async () => ({ observations: stream, stop })) },
        4096,
        async (_clientId, event) => {
          events.push(event)
          return 'delivered'
        }
      )
      const sender = trusted('terminal-retry-client')
      const bootstrapValue = await bootstrap(current, sender)
      await current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'scan.start', {
          serviceUuids: [],
          manufacturerData: [],
          localNamePrefix: null,
          deadline: null
        })
      )
      stream.push({
        kind: 'terminal',
        reason: 'closed',
        droppedItems: 0,
        droppedBytes: 0,
        replacedItems: 0
      })
      await flushAsyncWork()
      expect(stop).toHaveBeenCalledTimes(1)
      const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))
      expect(resources.scans.size).toBe(1)
      await jest.advanceTimersByTimeAsync(100)
      expect(stop).toHaveBeenCalledTimes(2)
      expect(resources.scans.size).toBe(0)
      expect(events.filter(event => event.item.kind === 'terminal')).toHaveLength(1)
      expectConsoleErrorMatching(
        '[ElectronRendererStreamRegistry] Failed to stop scan after source terminal:',
        expect.objectContaining({ handle: 'scan-1' })
      )
      await current.router.destroy()
    } finally {
      jest.useRealTimers()
    }
  })

  test('plans Electron scans from the trusted normalized query and never forwards renderer filters', async () => {
    const scanStream = createControlledStream()
    const scanStop = jest.fn(async () => {
      scanStream.close()
      return released()
    })
    const planScan = jest.fn(query =>
      snapshotScanPlan({
        sourceQuery: query,
        queryDigest: query.digest,
        residualQueryDigest: query.digest,
        nativeGuarantee: 'safe-superset',
        native: { predicates: [], complete: false },
        residual: { query, predicates: [], complete: true },
        unavailable: [],
        limitations: [],
        estimatedCost: 'high'
      })
    )
    const scan = jest.fn(async () => ({ observations: scanStream, stop: scanStop }))
    const current = createRouter({ planScan, scan })
    const sender = trusted('trusted-query-planner-client')
    const bootstrapValue = await bootstrap(current, sender)
    const query = normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Heart'] } }] })
    const wireQuery = {
      anyOf: [
        {
          peers: null,
          services: null,
          names: { exact: [], prefixes: ['Heart'] },
          manufacturerData: null,
          serviceData: null,
          rssi: null,
          connectable: null
        }
      ],
      exclude: null,
      digest: query.digest
    }

    const response = await current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'scan.start', {
        query: wireQuery,
        serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
        manufacturerData: [{ companyId: 76, dataPrefix: new Uint8Array([1]) }],
        localNamePrefix: 'renderer-controlled',
        deadline: null
      })
    )

    expect(response.payload.plan).toEqual(expect.objectContaining({ queryDigest: query.digest }))
    expect(response.payload.plan.nativeFilter).toBeUndefined()
    expect(response.payload.backendGeneration).toBe(String(current.attachment.backendGeneration))
    expect(planScan).toHaveBeenCalledWith(expect.objectContaining({ digest: query.digest }))
    expect(scan).toHaveBeenCalledWith(
      expect.objectContaining({
        query: expect.objectContaining({ digest: query.digest }),
        plan: expect.objectContaining({ queryDigest: query.digest }),
        filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null }
      })
    )

    await current.router.destroy()
  })

  test('fails closed for malformed queries, mismatched plan digests, and stale attachment generations', async () => {
    const current = createRouter()
    const sender = trusted('fail-closed-scan-planner-client')
    const bootstrapValue = await bootstrap(current, sender)

    const missingQuery = route(current, bootstrapValue, 1, 'scan.start', { deadline: null })
    delete missingQuery.envelope.payload.query
    await expect(current.router.dispatch(sender, missingQuery)).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'electron-main-router.scan-query' }
    })

    const mismatchedQuery = normalizeScanQuery({ anyOf: [{ names: { prefixes: ['Other'] } }] })
    const mismatchCurrent = createRouter({
      planScan: jest.fn(() => diagnosticPlan(mismatchedQuery)),
      scan: jest.fn()
    })
    const mismatchSender = trusted('mismatched-scan-plan-client')
    const mismatchBootstrap = await bootstrap(mismatchCurrent, mismatchSender)
    await expect(
      mismatchCurrent.router.dispatch(mismatchSender, route(mismatchCurrent, mismatchBootstrap, 1, 'scan.start', {}))
    ).rejects.toMatchObject({
      normalized: { code: 'protocol.violation', operation: 'electron-main-router.scan-plan-digest' }
    })

    const staleGeneration = route(current, bootstrapValue, 2, 'scan.start', { deadline: null })
    staleGeneration.envelope.attachment = {
      ...staleGeneration.envelope.attachment,
      backendGeneration: opaqueId('stale-generation', 'backend-generation', 'hardening')
    }
    await expect(current.router.dispatch(sender, staleGeneration)).rejects.toMatchObject({
      normalized: { code: 'protocol.violation', operation: 'electron-main-arbiter.attachment' }
    })

    await current.router.destroy()
    await mismatchCurrent.router.destroy()
  })
})

// PR210-36: the renderer's deadline is a `performance.now()` instant on the
// renderer process clock; Electron main has its own clock with a different
// time origin. What crosses the boundary is the remaining budget, measured just
// before send, and main admits it against its own clock at receipt.
describe('Electron deadline budget across process clocks', () => {
  function connectionFor(peerId) {
    return {
      peerId,
      connectionId: `${peerId}-connection`,
      connectionGeneration: `${peerId}-generation`,
      disconnect: jest.fn(async () => released())
    }
  }

  function rendererBootstrap(clientId) {
    const authority = createAuthority()
    return {
      attachment: authority.attachment,
      attachmentId: authority.attachment.attachmentId,
      versions: { ...authority.versions, ipcProtocol: negotiated('ipc-protocol') },
      capabilities: emptyCapabilitySnapshot(authority.attachment.backendGeneration),
      renderer: {
        clientId: opaqueId(clientId, 'client', `hardening:${clientId}`),
        windowScope: `${clientId}-window`,
        sessionScope: `${clientId}-session`
      },
      rendererLease: rendererLease(clientId)
    }
  }

  function recordingTransport(bootstrapValue) {
    const routed = []
    return {
      routed,
      invoke: jest.fn(async request => {
        if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrapValue }
        if (request.kind === 'route') {
          routed.push(request.envelope.payload)
          return { kind: 'route', payload: {} }
        }
        return { kind: 'release', cleanup: released() }
      }),
      acknowledge: jest.fn(async () => ({ kind: 'event.ack' })),
      subscribe: () => () => {}
    }
  }

  test('the renderer sends the remaining budget measured just before send, never its clock instant', async () => {
    const now = jest.spyOn(globalThis.performance, 'now').mockReturnValue(1_000)
    try {
      const transport = recordingTransport(rendererBootstrap('budget-renderer'))
      const client = new ElectronRendererBleClient(transport)
      await client.initialize()

      await client.request({
        command: 'connection.connect',
        payload: { peerId: 'a', deadline: 1_500.7 },
        binaryPayload: null
      })
      await client.request({
        command: 'connection.connect',
        payload: { peerId: 'b', deadline: 900 },
        binaryPayload: null
      })
      await client.request({
        command: 'connection.connect',
        payload: { peerId: 'c', deadline: null },
        binaryPayload: null
      })
      await client.request({ command: 'connection.connect', payload: { peerId: 'd' }, binaryPayload: null })

      expect(transport.routed).toEqual([
        { peerId: 'a', budgetMs: 500 },
        { peerId: 'b', budgetMs: 0 },
        { peerId: 'c' },
        { peerId: 'd' }
      ])
      await expect(
        client.request({
          command: 'connection.connect',
          payload: { peerId: 'e', deadline: 'soon' },
          binaryPayload: null
        })
      ).rejects.toMatchObject({ normalized: { code: 'protocol.malformed', operation: 'electron-renderer.deadline' } })
      await client.destroy()
    } finally {
      now.mockRestore()
    }
  })

  test.each([
    ['main clock far ahead of the renderer, short budget', 5_000_000, 50],
    ['main clock far behind the renderer, long budget', 3, 60_000]
  ])('main admits the budget against its own clock at receipt: %s', async (_label, mainNow, budgetMs) => {
    const connect = jest.fn(async peerId => connectionFor(String(peerId)))
    const current = createRouter({ connect, monotonicNow: () => mainNow })
    const sender = trusted(`budget-admission-${mainNow}`)
    const bootstrapValue = await bootstrap(current, sender)

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-budget', budgetMs })
      )
    ).resolves.toMatchObject({ kind: 'route', payload: { peerId: 'peer-budget' } })
    expect(connect).toHaveBeenCalledTimes(1)
    expect(Number(connect.mock.calls[0][1].deadline)).toBe(mainNow + budgetMs)
    await current.router.destroy()
  })

  test('an operation with no budget carries no deadline into main', async () => {
    const connect = jest.fn(async peerId => connectionFor(String(peerId)))
    const current = createRouter({ connect, monotonicNow: () => 42 })
    const sender = trusted('budget-absent')
    const bootstrapValue = await bootstrap(current, sender)

    await current.router.dispatch(sender, route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'p' }))
    expect(connect.mock.calls[0][1].deadline).toBeNull()
    await current.router.destroy()
  })

  test('an already-expired budget times out in main with zero effects', async () => {
    const connect = jest.fn()
    const write = jest.fn()
    const current = createRouter({ connect, monotonicNow: () => 7_000 })
    const sender = trusted('budget-expired')
    const bootstrapValue = await bootstrap(current, sender)
    const resources = current.router.resources.get(String(bootstrapValue.rendererLease.leaseId))
    resources.databases.set('database-budget', {
      database: { write },
      characteristics: new Map([['characteristic-1', { characteristicUuid: 'characteristic-1' }]]),
      descriptors: new Map()
    })

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-expired', budgetMs: 0 })
      )
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out', retryability: 'caller-decides' } })
    const writeRequest = route(current, bootstrapValue, 2, 'gatt.write', {
      databaseHandle: 'database-budget',
      characteristicHandle: 'characteristic-1',
      mode: 'with-response',
      budgetMs: 0
    })
    writeRequest.envelope.binaryPayload = new Uint8Array([1])
    await expect(current.router.dispatch(sender, writeRequest)).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out', retryability: 'caller-decides' }
    })
    expect(connect).not.toHaveBeenCalled()
    expect(write).not.toHaveBeenCalled()
    expect(resources.connections).toHaveProperty('size', 0)
    await current.router.destroy()
  })

  test('main counts time spent after receipt against the admitted budget', async () => {
    let mainNow = 1_000
    const connectResult = deferred()
    const connection = connectionFor('peer-queued')
    const connect = jest.fn(() => connectResult.promise)
    const current = createRouter({ connect, monotonicNow: () => mainNow })
    const sender = trusted('budget-queued')
    const bootstrapValue = await bootstrap(current, sender)

    const pending = current.router.dispatch(
      sender,
      route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-queued', budgetMs: 10 })
    )
    await flushAsyncWork()
    mainNow = 1_011
    connectResult.resolve(connection)
    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
    expect(connection.disconnect).toHaveBeenCalledTimes(1)
    await current.router.destroy()
  })

  test.each([
    ['an absolute renderer deadline', { deadline: 10 }],
    ['a negative budget', { budgetMs: -1 }],
    ['a fractional budget', { budgetMs: 1.5 }],
    ['a non-numeric budget', { budgetMs: '10' }]
  ])('main fails closed on %s with zero effects', async (_label, timing) => {
    const connect = jest.fn()
    const current = createRouter({ connect, monotonicNow: () => 0 })
    const sender = trusted(`budget-malformed-${Object.keys(timing)[0]}-${String(Object.values(timing)[0])}`)
    const bootstrapValue = await bootstrap(current, sender)

    await expect(
      current.router.dispatch(
        sender,
        route(current, bootstrapValue, 1, 'connection.connect', { peerId: 'peer-malformed', ...timing })
      )
    ).rejects.toMatchObject({ normalized: { code: 'protocol.malformed', operation: 'electron-main-router.budget' } })
    expect(connect).not.toHaveBeenCalled()
    await current.router.destroy()
  })

  test.each([
    ['renderer clock near zero, main clock far ahead', 10, 1_000_000_000],
    ['renderer clock far ahead, main clock near zero', 1_000_000_000, 10]
  ])('renderer to main end to end with offset clocks: %s', async (_label, rendererNow, mainNow) => {
    const now = jest.spyOn(globalThis.performance, 'now').mockReturnValue(rendererNow)
    try {
      const connect = jest.fn(async peerId => connectionFor(String(peerId)))
      const current = createRouter({ connect, monotonicNow: () => mainNow })
      const sender = trusted(`budget-e2e-${rendererNow}`)
      const transport = {
        invoke: jest.fn(async request => {
          if (request.kind === 'route')
            return current.router.dispatch(sender, { kind: 'route', envelope: request.envelope })
          return current.router.dispatch(sender, request)
        }),
        acknowledge: jest.fn(async () => ({ kind: 'event.ack' })),
        subscribe: () => () => {}
      }
      const client = new ElectronRendererBleClient(transport)
      await client.initialize()

      await expect(
        client.request({
          command: 'connection.connect',
          payload: { peerId: 'peer-e2e', deadline: rendererNow + 1_000 },
          binaryPayload: null
        })
      ).resolves.toMatchObject({ payload: { peerId: 'peer-e2e' } })
      expect(Number(connect.mock.calls[0][1].deadline)).toBe(mainNow + 1_000)

      await expect(
        client.request({
          command: 'connection.connect',
          payload: { peerId: 'peer-e2e-expired', deadline: rendererNow - 1 },
          binaryPayload: null
        })
      ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
      expect(connect).toHaveBeenCalledTimes(1)
      await current.router.destroy()
    } finally {
      now.mockRestore()
    }
  })
})

// PR210-73: the renderer/main wire changed (relative `budgetMs`, `commit` on
// errors), so the IPC protocol is 3. A mixed pair fails at bootstrap as
// protocol.incompatible, in both directions, before any operation.
describe('Electron IPC protocol version 3', () => {
  const { IPC_PROTOCOL_VERSION } = require('../src/ipc/protocol')
  const { negotiateVersion } = require('../src/backend-contract/primitives')
  const { BackendContractError } = require('../src/backend-contract/errors')

  function protocolRange(value) {
    return versionRange(version('ipc-protocol', value), version('ipc-protocol', value))
  }

  function negotiatedIpc(value) {
    const range = protocolRange(value)
    return { axis: 'ipc-protocol', selected: version('ipc-protocol', value), localRange: range, remoteRange: range }
  }

  function clientBootstrap(ipcProtocolValue) {
    const authority = createAuthority()
    return {
      attachment: authority.attachment,
      attachmentId: authority.attachment.attachmentId,
      versions: { ...authority.versions, ipcProtocol: negotiatedIpc(ipcProtocolValue) },
      capabilities: emptyCapabilitySnapshot(authority.attachment.backendGeneration),
      renderer: {
        clientId: opaqueId('protocol-client', 'client', 'hardening:protocol'),
        windowScope: 'protocol-window',
        sessionScope: 'protocol-session'
      },
      rendererLease: rendererLease('protocol-client')
    }
  }

  test('the renderer offers exactly IPC protocol 4', () => {
    expect(IPC_PROTOCOL_VERSION).toBe(4)
    expect(IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol).toEqual(protocolRange(4))
  })

  test('new main refuses an old renderer at bootstrap with no lease and no effects', async () => {
    const connect = jest.fn()
    const current = createRouter({ connect })
    const sender = trusted('old-renderer')
    const oldOffer = { ...IPC_CLIENT_COMPATIBILITY_OFFER, ipcProtocol: protocolRange(3) }

    await expect(current.router.dispatch(sender, { kind: 'bootstrap', offer: oldOffer })).rejects.toMatchObject({
      normalized: { code: 'protocol.incompatible' }
    })
    expect(current.router.resources).toHaveProperty('size', 0)
    expect(connect).not.toHaveBeenCalled()

    const response = await current.router.dispatch(sender, { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
    expect(response.bootstrap.versions.ipcProtocol.selected).toEqual(version('ipc-protocol', 4))
    await current.router.destroy()
  })

  test('new renderer refuses an old main that cannot negotiate protocol 4', async () => {
    const transport = {
      invoke: jest.fn(async request => {
        if (request.kind !== 'bootstrap') throw new Error(`unexpected ${request.kind}`)
        try {
          negotiateVersion(protocolRange(3), request.offer.ipcProtocol)
        } catch (error) {
          if (error instanceof BackendContractError) return { kind: 'failure', error: error.normalized }
          throw error
        }
        return { kind: 'bootstrap', bootstrap: clientBootstrap(3) }
      }),
      acknowledge: jest.fn(),
      subscribe: () => () => {}
    }
    const client = new ElectronRendererBleClient(transport)

    await expect(client.initialize()).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
    expect(transport.invoke.mock.calls.map(([request]) => request.kind)).toEqual(['bootstrap'])
  })

  test('new renderer refuses a bootstrap that selected protocol 3 and releases its lease', async () => {
    const transport = {
      invoke: jest.fn(async request => {
        if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: clientBootstrap(3) }
        if (request.kind === 'release') return { kind: 'release', cleanup: released() }
        throw new Error(`unexpected ${request.kind}`)
      }),
      acknowledge: jest.fn(),
      subscribe: () => () => {}
    }
    const client = new ElectronRendererBleClient(transport)

    await expect(client.initialize()).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
    expect(transport.invoke.mock.calls.map(([request]) => request.kind)).toEqual(['bootstrap', 'release'])
  })
})
