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

async function createAdmissionHarness(options = {}) {
  const commands = []
  const disconnectPayloads = []
  const unsubscribePayloads = []
  const bootstrap = bootstrapRecord()
  const connectPayload = options.connectPayload ?? validConnectPayload()
  const subscribePayload = options.subscribePayload
  let disconnectImpl =
    options.disconnect ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  let unsubscribeImpl =
    options.unsubscribe ??
    (async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }))
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const command = request.envelope.command
      const payload = request.envelope.payload
      commands.push(command)
      if (command === 'connection.connect') return { kind: 'route', payload: connectPayload }
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
    setDisconnect(next) {
      disconnectImpl = next
    },
    setUnsubscribe(next) {
      unsubscribeImpl = next
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
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toEqual({
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
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toEqual({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
    await harness.ipc.destroy()
    expect(inspectIpcProvisionalAdmissionForTests(harness.ipc)).toEqual({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
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
