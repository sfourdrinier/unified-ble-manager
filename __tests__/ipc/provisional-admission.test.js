const {
  IpcBleManager,
  inspectIpcProvisionalAdmissionForTests
} = require('../../src/ipc/manager')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

function negotiated(axis, value = axis === 'ipc-protocol' ? 2 : 1) {
  const selected = { axis, value }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function bootstrapRecord() {
  const backendGeneration = 'backend-generation-1'
  return {
    attachment: {
      attachmentId: 'attachment-1',
      backendInstanceId: 'backend-1',
      backendGeneration,
      adapter: {
        adapterId: 'adapter-1',
        displayName: 'Bluetooth',
        state: {
          availability: 'available',
          authorization: 'granted',
          power: 'on',
          heard: null,
          backendGeneration,
          updatedAt: 1,
          safeReason: null
        },
        adapterGeneration: 'adapter-generation-1',
        limitations: []
      }
    },
    attachmentId: 'attachment-1',
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
        id,
        state: 'unsupported',
        selectedSchemaRange: negotiated('capability-schema').localRange,
        implementationOrigin: 'backend-native',
        tck: {
          suiteId: 'capability.catalog-v2',
          requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
          contractRange: negotiated('capability-schema').localRange
        },
        evidence: {
          receiptId: `fixture-${id}`,
          evidenceLevel: 'blocked',
          implementationVersion: 'fixture',
          sourceDigest: `fixture-${id}`,
          scenarioIds: ['capability.truth-limits-evidence-and-binding'],
          limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }]
        },
        limitations: [{ code: 'not-implemented', explanation: 'fixture', affectedGuarantee: 'support' }],
        limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
      }))
    },
    renderer: { clientId: 'client-1', windowScope: 'window', sessionScope: 'session' },
    rendererLease: { leaseId: 'lease-1', generation: 'lease-generation-1' }
  }
}

function validConnectPayload(overrides = {}) {
  return {
    handle: 'connection-1',
    connectionId: 'connection-id-1',
    ownerLeaseId: 'lease-1',
    peerId: 'peer-1',
    connectionGeneration: 'generation-1',
    ...overrides
  }
}

function validGattDiscoverPayload(overrides = {}) {
  return {
    schemaVersion: 2,
    handle: 'database-1',
    databaseId: 'database-id-1',
    databaseGeneration: 'database-generation-1',
    services: [{ uuid: '180d', occurrence: '0', primary: true, includedServices: [] }],
    characteristics: [
      {
        handle: 'characteristic-1',
        serviceUuid: '180d',
        serviceOccurrence: '0',
        characteristicUuid: '2a37',
        characteristicOccurrence: '0',
        properties: ['notify', 'read']
      }
    ],
    descriptors: [
      { handle: 'descriptor-1', characteristicHandle: 'characteristic-1', uuid: '2901', occurrence: '0' }
    ],
    ...overrides
  }
}

async function createAdmissionHarness(options = {}) {
  const commands = []
  const disconnectPayloads = []
  const unsubscribePayloads = []
  const gattUnsubscribePayloads = []
  const databaseReleasePayloads = []
  const bootstrap = bootstrapRecord()
  const connectPayload = options.connectPayload ?? validConnectPayload()
  const subscribePayload = options.subscribePayload
  const discoverPayload = options.discoverPayload ?? validGattDiscoverPayload()
  const gattSubscribePayload = options.gattSubscribePayload ?? { handle: 'subscription-1' }
  let disconnectImpl =
    options.disconnect ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  let unsubscribeImpl =
    options.unsubscribe ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  let gattUnsubscribeImpl =
    options.gattUnsubscribe ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  let databaseReleaseImpl =
    options.databaseRelease ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const command = request.envelope.command
      const payload = request.envelope.payload
      commands.push(command)
      if (command === 'connection.connect') {
        if (typeof options.onConnect === 'function') options.onConnect()
        return { kind: 'route', payload: connectPayload }
      }
      if (command === 'connection.disconnect') {
        disconnectPayloads.push(payload)
        return disconnectImpl()
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload:
            subscribePayload ?? {
              handle: payload.connectionEventsHandle,
              connectionId: payload.connectionId,
              connectionGeneration: payload.connectionGeneration,
              eventSchemaVersion: 2
            }
        }
      }
      if (command === 'connection.events.ready') {
        if (options.readyPayload !== undefined) return { kind: 'route', payload: options.readyPayload }
        return { kind: 'route', payload: { state: 'ready' } }
      }
      if (command === 'connection.events.unsubscribe') {
        unsubscribePayloads.push(payload)
        return unsubscribeImpl()
      }
      if (command === 'gatt.discover') {
        return { kind: 'route', payload: typeof discoverPayload === 'function' ? discoverPayload() : discoverPayload }
      }
      if (command === 'gatt.database.release') {
        databaseReleasePayloads.push(payload)
        return databaseReleaseImpl()
      }
      if (command === 'gatt.subscribe') {
        return {
          kind: 'route',
          payload: typeof gattSubscribePayload === 'function' ? gattSubscribePayload() : gattSubscribePayload
        }
      }
      if (command === 'gatt.unsubscribe') {
        gattUnsubscribePayloads.push(payload)
        return gattUnsubscribeImpl()
      }
      return { kind: 'route', payload: { state: 'released', failures: [] } }
    },
    subscribe() {
      return () => undefined
    },
    acknowledge: async () => ({ kind: 'event.ack' })
  }
  const ipc = await IpcBleManager.create(transport)
  return {
    ipc,
    commands,
    disconnectPayloads,
    unsubscribePayloads,
    gattUnsubscribePayloads,
    databaseReleasePayloads,
    setDisconnect(next) {
      disconnectImpl = next
    },
    setUnsubscribe(next) {
      unsubscribeImpl = next
    },
    setGattUnsubscribe(next) {
      gattUnsubscribeImpl = next
    }
  }
}

describe('IPC provisional admission', () => {
  test('mismatched connect identity still disconnects the host handle', async () => {
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ peerId: 'other-peer' })
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
      normalized: { code: 'protocol.violation' }
    })
    expect(harness.commands).toContain('connection.disconnect')
    expect(harness.disconnectPayloads[0]).toMatchObject({
      connectionHandle: 'connection-1',
      peerId: 'other-peer',
      ownerLeaseId: 'lease-1',
      connectionId: 'connection-id-1',
      connectionGeneration: 'generation-1'
    })
    await harness.ipc.destroy()
  })

  test('missing connectionId still disconnects using the returned handle and remaining host identity', async () => {
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ connectionId: undefined })
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    expect(harness.disconnectPayloads[0]).toMatchObject({
      connectionHandle: 'connection-1',
      peerId: 'peer-1',
      ownerLeaseId: 'lease-1',
      connectionGeneration: 'generation-1'
    })
    expect(harness.disconnectPayloads[0].connectionId).toBeUndefined()
    await harness.ipc.destroy()
  })

  test('missing connectionGeneration still disconnects using the returned handle and remaining host identity', async () => {
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ connectionGeneration: undefined })
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    expect(harness.disconnectPayloads[0]).toMatchObject({
      connectionHandle: 'connection-1',
      peerId: 'peer-1'
    })
    expect(harness.disconnectPayloads[0].connectionGeneration).toBeUndefined()
    await harness.ipc.destroy()
  })

  test('missing handle fails closed and manager destroy releases the host lease resources', async () => {
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ handle: undefined })
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    expect(harness.commands).not.toContain('connection.disconnect')
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc).unresolvedConnectionCount).toBe(1)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toMatchObject({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
  })

  test('malformed events subscribe still unsubscribes', async () => {
    const harness = await createAdmissionHarness({
      subscribePayload: { handle: 'wrong-handle', eventSchemaVersion: 1 }
    })
    const connection = await harness.ipc.connect('peer-1')
    await expect(harness.ipc.subscribeConnectionEvents(connection.handle, {
      connectionId: connection.connectionId,
      connectionGeneration: connection.connectionGeneration
    })).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
    expect(harness.commands).toContain('connection.events.unsubscribe')
    await connection.release()
    await harness.ipc.destroy()
  })

  test('unsubscribe release-failed is preserved on admission failure', async () => {
    const unsubscribeCleanup = {
      state: 'release-failed',
      failures: [
        {
          resourceKind: 'connection-events',
          error: {
            code: 'platform.failure',
            domain: 'connection',
            operation: 'ipc-manager.connection-events-unsubscribe',
            platform: null,
            retryability: 'caller-decides'
          }
        }
      ]
    }
    const harness = await createAdmissionHarness({
      subscribePayload: { handle: 'wrong-handle', eventSchemaVersion: 1 },
      unsubscribe: async () => ({ kind: 'route', payload: unsubscribeCleanup })
    })
    const connection = await harness.ipc.connect('peer-1')
    await expect(
      harness.ipc.subscribeConnectionEvents(connection.handle, {
        connectionId: connection.connectionId,
        connectionGeneration: connection.connectionGeneration
      })
    ).rejects.toMatchObject({
      errors: expect.arrayContaining([expect.objectContaining({ cleanup: unsubscribeCleanup })])
    })
    await connection.release()
    await harness.ipc.destroy()
  })

  test('failed provisional cleanup is retried by connection release or manager destroy', async () => {
    let disconnectCalls = 0
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ peerId: 'other-peer' }),
      disconnect: async () => {
        disconnectCalls += 1
        if (disconnectCalls === 1) throw new Error('disconnect-failed')
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({ name: 'AggregateError' })
    expect(disconnectCalls).toBe(1)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(disconnectCalls).toBe(2)
  })

  test('valid connect publishes one connection without compensating disconnect', async () => {
    const harness = await createAdmissionHarness()
    const connection = await harness.ipc.connect('peer-1')
    expect(connection.handle).toBe('connection-1')
    expect(harness.commands).toEqual(['connection.connect'])
    expect(harness.disconnectPayloads).toEqual([])
    await connection.release()
    await harness.ipc.destroy()
  })

  test('successful compensation and destroy return provisional resource counters to zero', async () => {
    const harness = await createAdmissionHarness({
      connectPayload: validConnectPayload({ peerId: 'other-peer' })
    })
    await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
      normalized: { code: 'protocol.violation' }
    })
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toMatchObject({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
    await harness.ipc.destroy()
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toMatchObject({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
  })

  test('expired connect deadline does not invoke the transport', async () => {
    const harness = await createAdmissionHarness()
    await expect(harness.ipc.connect('peer-1', { deadline: globalThis.performance.now() - 1 })).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(harness.commands).not.toContain('connection.connect')
    await harness.ipc.destroy()
  })

  test('expired scan.start deadline does not invoke the transport', async () => {
    const harness = await createAdmissionHarness()
    await expect(harness.ipc.scan({ deadline: globalThis.performance.now() - 1 })).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(harness.commands).not.toContain('scan.start')
    await harness.ipc.destroy()
  })

  test('expired gatt.discover and gatt.subscribe deadlines do not invoke the transport', async () => {
    const harness = await createAdmissionHarness()
    const expired = { deadline: globalThis.performance.now() - 1 }
    await expect(harness.ipc.route('gatt.discover', expired)).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    await expect(harness.ipc.route('gatt.subscribe', expired)).rejects.toMatchObject({
      normalized: { code: 'operation.timed-out' }
    })
    expect(harness.commands).not.toContain('gatt.discover')
    expect(harness.commands).not.toContain('gatt.subscribe')
    await harness.ipc.destroy()
  })

  test('already-aborted signal does not invoke the transport', async () => {
    const harness = await createAdmissionHarness()
    const controller = new AbortController()
    controller.abort()
    await expect(harness.ipc.connect('peer-1', { signal: controller.signal })).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    expect(harness.commands).not.toContain('connection.connect')
    await harness.ipc.destroy()
  })

  test('future deadline still dispatches', async () => {
    const harness = await createAdmissionHarness()
    const connection = await harness.ipc.connect('peer-1', { deadline: globalThis.performance.now() + 10_000 })
    expect(harness.commands).toContain('connection.connect')
    expect(connection.handle).toBe('connection-1')
    await connection.release()
    await harness.ipc.destroy()
  })

  test('deadline expiring after dispatch compensates a resource-bearing success', async () => {
    let now = 1_000
    const originalNow = globalThis.performance.now.bind(globalThis.performance)
    globalThis.performance.now = () => now
    try {
      const harness = await createAdmissionHarness({
        onConnect() {
          now = 2_000
        }
      })
      await expect(harness.ipc.connect('peer-1', { deadline: 1_500 })).rejects.toMatchObject({
        normalized: { code: 'operation.timed-out' }
      })
      expect(harness.commands).toContain('connection.connect')
      expect(harness.commands).toContain('connection.disconnect')
      await harness.ipc.destroy()
    } finally {
      globalThis.performance.now = originalNow
    }
  })

  test.each(['electron', 'tauri'])(
    '%s transport doubles share the pre-dispatch deadline guard',
    async () => {
      const harness = await createAdmissionHarness()
      await expect(
        harness.ipc.connect('peer-1', { deadline: globalThis.performance.now() - 1 })
      ).rejects.toMatchObject({
        normalized: { code: 'operation.timed-out' }
      })
      expect(harness.commands).not.toContain('connection.connect')
      await harness.ipc.destroy()
    }
  )

  test('duplicate GATT subscribe handle rejects without unsubscribing the admitted subscription', async () => {
    const harness = await createAdmissionHarness({
      gattSubscribePayload: { handle: 'subscription-1' }
    })
    const connection = await harness.ipc.connect('peer-1')
    const database = await connection.discover()
    const characteristic = database.characteristics[0]
    const first = await characteristic.subscribe()
    await expect(characteristic.subscribe()).rejects.toMatchObject({
      normalized: { code: 'protocol.violation', operation: 'ipc-manager.stream-handle' }
    })
    expect(harness.commands.filter(command => command === 'gatt.unsubscribe')).toHaveLength(0)
    expect(first.handle).toBe('subscription-1')
    await first.remove()
    expect(harness.commands.filter(command => command === 'gatt.unsubscribe')).toHaveLength(1)
    await connection.release()
    await harness.ipc.destroy()
  })

  test('missing GATT subscribe handle is fail-closed and destroy settles provisional ownership', async () => {
    const harness = await createAdmissionHarness({
      gattSubscribePayload: {}
    })
    const connection = await harness.ipc.connect('peer-1')
    const database = await connection.discover()
    const characteristic = database.characteristics[0]
    await expect(characteristic.subscribe()).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    expect(harness.commands).not.toContain('gatt.unsubscribe')
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc).unresolvedGattSubscriptionCount).toBe(1)
    await expect(harness.ipc.destroy()).resolves.toEqual({ state: 'released', failures: [] })
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc).unresolvedGattSubscriptionCount).toBe(0)
    await connection.release().catch(() => undefined)
  })

  test('malformed discovered GATT topology releases the provisional database handle', async () => {
    const harness = await createAdmissionHarness({
      discoverPayload: validGattDiscoverPayload({
        descriptors: [{ handle: 'descriptor-1', characteristicHandle: 'missing-char', uuid: '2901', occurrence: '0' }]
      })
    })
    const connection = await harness.ipc.connect('peer-1')
    await expect(connection.discover()).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    expect(harness.commands).toContain('gatt.database.release')
    expect(harness.databaseReleasePayloads[0]).toMatchObject({ databaseHandle: 'database-1' })
    await connection.release()
    await harness.ipc.destroy()
  })

  test('rediscovery unsubscribes old subscriptions instead of synthesizing released', async () => {
    let discoverCount = 0
    const harness = await createAdmissionHarness({
      discoverPayload: () => {
        discoverCount += 1
        return validGattDiscoverPayload({
          handle: `database-${discoverCount}`,
          databaseGeneration: `database-generation-${discoverCount}`,
          ...(discoverCount > 1 ? { rediscoveryReason: 'manual-rediscovery' } : {})
        })
      }
    })
    const connection = await harness.ipc.connect('peer-1')
    const database = await connection.discover()
    const subscription = await database.characteristics[0].subscribe()
    const replacement = await connection.rediscoverGatt({}, 'manual-rediscovery')
    expect(harness.commands).toContain('gatt.unsubscribe')
    expect(harness.gattUnsubscribePayloads[0]).toMatchObject({ subscriptionHandle: 'subscription-1' })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'released', failures: [] })
    expect(replacement.handle).toBe('database-2')
    await connection.release()
    await harness.ipc.destroy()
  })

  test.each(['electron', 'tauri'])(
    '%s transport doubles exercise the same admission helper',
    async host => {
      const harness = await createAdmissionHarness({
        connectPayload: validConnectPayload({ peerId: `${host}-peer` })
      })
      await expect(harness.ipc.connect('peer-1')).rejects.toMatchObject({
        normalized: { code: 'protocol.violation' }
      })
      expect(harness.disconnectPayloads[0]).toMatchObject({
        connectionHandle: 'connection-1',
        peerId: `${host}-peer`
      })
      await harness.ipc.destroy()
    }
  )
})
