const {
  IpcBleManager,
  inspectIpcProvisionalAdmissionForTests
} = require('../../src/ipc/manager')
const { BackendContractError } = require('../../src/backend-contract/errors')
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

function transportError(operation) {
  return new BackendContractError({
    code: 'platform.transport',
    domain: 'connection',
    operation,
    platform: null,
    retryability: 'caller-decides'
  })
}

async function createConnectedIpc(behavior) {
  const commands = []
  const bootstrap = bootstrapRecord()
  const transport = {
    invoke: async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const command = request.envelope.command
      const payload = request.envelope.payload
      commands.push(command)
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-1',
            connectionId: 'connection-id-1',
            ownerLeaseId: 'lease-1',
            peerId: 'peer-1',
            connectionGeneration: 'generation-1'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: payload.connectionEventsHandle,
            connectionId: payload.connectionId,
            connectionGeneration: payload.connectionGeneration,
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.events.unsubscribe') return behavior.unsubscribe()
      if (command === 'connection.disconnect') return behavior.disconnect()
      return { kind: 'route', payload: { state: 'released', failures: [] } }
    },
    subscribe() {
      return () => undefined
    },
    acknowledge: async () => ({ kind: 'event.ack' })
  }
  const ipc = await IpcBleManager.create(transport)
  const connection = await ipc.connect('peer-1')
  void connection.events
  for (let attempt = 0; attempt < 30 && !commands.includes('connection.events.ready'); attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
  expect(commands).toContain('connection.events.ready')
  return { ipc, connection, commands }
}

describe('IPC connection cleanup independence', () => {
  test('unsubscribe rejection does not suppress connection.disconnect', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => {
        throw transportError('ipc-manager.connection-events-unsubscribe')
      },
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const result = await connection.release()
    expect(commands).toEqual(
      expect.arrayContaining(['connection.events.unsubscribe', 'connection.disconnect'])
    )
    expect(commands.indexOf('connection.disconnect')).toBeGreaterThan(
      commands.indexOf('connection.events.unsubscribe')
    )
    expect(result.state).toBe('release-failed')
    expect(result.failures.some(failure => failure.resourceKind === 'connection-events')).toBe(true)
    await ipc.destroy()
  })

  test('preserves both cleanup failures', async () => {
    const { ipc, connection } = await createConnectedIpc({
      unsubscribe: async () => {
        throw transportError('ipc-manager.connection-events-unsubscribe')
      },
      disconnect: async () => {
        throw transportError('ipc-manager.connection-disconnect')
      }
    })
    const result = await connection.release().then(
      value => value,
      error => error
    )
    if (result instanceof AggregateError) {
      expect(result.errors).toHaveLength(2)
    } else {
      expect(result.state).toBe('release-failed')
      const kinds = result.failures.map(failure => failure.resourceKind)
      expect(kinds).toEqual(expect.arrayContaining(['connection-events', 'connection']))
    }
    await ipc.destroy()
  })

  test('unsubscribe release-failed still attempts physical disconnect', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => ({
        kind: 'route',
        payload: {
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
      }),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const result = await connection.release()
    expect(commands).toContain('connection.disconnect')
    expect(result.state).toBe('release-failed')
    expect(result.failures.some(failure => failure.resourceKind === 'connection-events')).toBe(true)
    const before = commands.filter(command => command === 'connection.disconnect').length
    const retry = await connection.release()
    expect(retry.state).toBe('release-failed')
    expect(commands.filter(command => command === 'connection.disconnect').length).toBe(before)
    expect(commands.filter(command => command === 'connection.events.unsubscribe').length).toBeGreaterThan(1)
    await ipc.destroy()
  })

  test('failed physical disconnect remains retryable after lifecycle cleanup succeeds', async () => {
    let disconnectAttempts = 0
    const { ipc, connection } = await createConnectedIpc({
      unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
      disconnect: async () => {
        disconnectAttempts += 1
        if (disconnectAttempts === 1) {
          return {
            kind: 'route',
            payload: {
              state: 'release-failed',
              failures: [
                {
                  resourceKind: 'connection',
                  error: {
                    code: 'connection.lost',
                    domain: 'connection',
                    operation: 'ipc-manager.connection-disconnect',
                    platform: null,
                    retryability: 'caller-decides'
                  }
                }
              ]
            }
          }
        }
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
    })
    await expect(connection.release()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(disconnectAttempts).toBe(2)
    await ipc.destroy()
  })

  test('concurrent release shares one teardown attempt', async () => {
    let disconnects = 0
    const { ipc, connection } = await createConnectedIpc({
      unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
      disconnect: async () => {
        disconnects += 1
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
    })
    const [first, second] = await Promise.all([connection.release(), connection.release()])
    expect(first.state).toBe('released')
    expect(second.state).toBe('released')
    expect(disconnects).toBe(1)
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(disconnects).toBe(1)
    await ipc.destroy()
  })

  async function createHungAdmission() {
    const commands = []
    const bootstrap = bootstrapRecord()
    let resolveSubscribe
    const subscribeGate = new Promise(resolve => {
      resolveSubscribe = resolve
    })
    const transport = {
      invoke: async request => {
        if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap }
        if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
        const command = request.envelope.command
        const payload = request.envelope.payload
        commands.push(command)
        if (command === 'connection.connect') {
          return {
            kind: 'route',
            payload: {
              handle: 'connection-1',
              connectionId: 'connection-id-1',
              ownerLeaseId: 'lease-1',
              peerId: 'peer-1',
              connectionGeneration: 'generation-1'
            }
          }
        }
        if (command === 'connection.events.subscribe') {
          await subscribeGate
          return {
            kind: 'route',
            payload: {
              handle: payload.connectionEventsHandle,
              connectionId: payload.connectionId,
              connectionGeneration: payload.connectionGeneration,
              eventSchemaVersion: 2
            }
          }
        }
        if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
        if (command === 'connection.events.unsubscribe') {
          return { kind: 'route', payload: { state: 'released', failures: [] } }
        }
        if (command === 'connection.disconnect') {
          return { kind: 'route', payload: { state: 'released', failures: [] } }
        }
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      },
      subscribe() {
        return () => undefined
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }
    const ipc = await IpcBleManager.create(transport)
    const connection = await ipc.connect('peer-1')
    void connection.events
    await new Promise(resolve => setImmediate(resolve))
    return {
      ipc,
      connection,
      commands,
      releaseSubscribe() {
        resolveSubscribe(undefined)
      }
    }
  }

  async function flushMicrotasks() {
    for (let attempt = 0; attempt < 20; attempt += 1) {
      await new Promise(resolve => setImmediate(resolve))
    }
  }

  test('release completes while connection.events.subscribe never settles', async () => {
    const { ipc, connection, commands } = await createHungAdmission()
    let released = false
    const release = connection.release().then(result => {
      released = true
      return result
    })
    await flushMicrotasks()
    expect(released).toBe(true)
    await expect(release).resolves.toMatchObject({ state: 'released' })
    expect(commands).toContain('connection.disconnect')
    await ipc.destroy()
  })

  test('connection.disconnect is still routed', async () => {
    const { ipc, connection, commands } = await createHungAdmission()
    await connection.release()
    expect(commands.filter(command => command === 'connection.disconnect')).toEqual(['connection.disconnect'])
    await ipc.destroy()
  })

  test('late subscribe success is unsubscribed and does not resurrect the connection', async () => {
    const { ipc, connection, commands, releaseSubscribe } = await createHungAdmission()
    await connection.release()
    expect(commands).toContain('connection.disconnect')
    expect(commands).not.toContain('connection.events.unsubscribe')
    releaseSubscribe()
    await flushMicrotasks()
    expect(commands).toContain('connection.events.unsubscribe')
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
    await ipc.destroy()
  })

  test('concurrent release calls share one teardown', async () => {
    const { ipc, connection, commands } = await createHungAdmission()
    const [first, second] = await Promise.all([connection.release(), connection.release()])
    expect(first.state).toBe('released')
    expect(second.state).toBe('released')
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
    await ipc.destroy()
  })

  test('manager destroy cannot hang behind lifecycle admission', async () => {
    const { ipc, connection } = await createHungAdmission()
    void connection.events
    let destroyed = false
    const destroy = ipc.destroy().then(result => {
      destroyed = true
      return result
    })
    await flushMicrotasks()
    expect(destroyed).toBe(true)
    await expect(destroy).resolves.toMatchObject({ state: 'released' })
  })

  test('late admission compensation and destroy return connection-event counters to zero', async () => {
    const { ipc, connection, releaseSubscribe } = await createHungAdmission()
    await connection.release()
    releaseSubscribe()
    await flushMicrotasks()
    await ipc.destroy()
    expect(inspectIpcProvisionalAdmissionForTests(ipc)).toMatchObject({
      unresolvedConnectionCount: 0,
      unresolvedEventSubscriptionCount: 0
    })
  })

  test('manager destroy remains safe after unsubscribe failure and successful disconnect', async () => {
    const { ipc, connection } = await createConnectedIpc({
      unsubscribe: async () => {
        throw transportError('ipc-manager.connection-events-unsubscribe')
      },
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    await connection.release()
    await expect(ipc.destroy()).resolves.toMatchObject({ state: 'released' })
  })
})
