const { IpcBleManager, IpcConnection, inspectIpcProvisionalAdmissionForTests } = require('../../src/ipc/manager')
const { BackendContractError } = require('../../src/backend-contract/errors')
const { BUILT_IN_FEATURE_IDS } = require('../../src/backend-contract/capabilities')

function negotiated(axis, value = axis === 'ipc-protocol' ? 4 : 1) {
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
      if (command === 'connection.events.ready')
        return behavior.ready?.() ?? { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.events.unsubscribe') return behavior.unsubscribe()
      if (command === 'connection.disconnect') return behavior.disconnect()
      if (command === 'gatt.discover')
        return {
          kind: 'route',
          payload: {
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
            descriptors: []
          }
        }
      if (command === 'gatt.subscribe')
        return (
          behavior.gattSubscribe?.() ?? {
            kind: 'route',
            payload: { handle: 'subscription-1', observedDelivery: 'unknown' }
          }
        )
      if (command === 'gatt.unsubscribe') return behavior.gattUnsubscribe()
      if (command === 'connection.rssi') return { kind: 'route', payload: { rssi: -42 } }
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
  test('late GATT admission after confirmed parent rejects without another native cleanup request', async () => {
    let admit
    const pending = new Promise(resolve => {
      admit = resolve
    })
    const { ipc, connection, commands } = await createConnectedIpc({
      gattSubscribe: () => pending,
      gattUnsubscribe: () => new Promise(() => {}),
      unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const database = await connection.discover()
    const subscription = database.characteristics[0].subscribe()
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    admit({ kind: 'route', payload: { handle: 'late-subscription', observedDelivery: 'unknown' } })
    await expect(subscription).rejects.toMatchObject({ normalized: { code: 'gatt.stale-handle' } })
    expect(commands).not.toContain('gatt.unsubscribe')
    await ipc.destroy()
  })

  test('late ready rejection after confirmed parent does not start another unsubscribe', async () => {
    let refuseReady
    const ready = new Promise((_resolve, reject) => {
      refuseReady = reject
    })
    const { ipc, connection, commands } = await createConnectedIpc({
      ready: () => ready,
      unsubscribe: () => new Promise(() => {}),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    refuseReady(transportError('ready'))
    await new Promise(resolve => setImmediate(resolve))
    expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
    await ipc.destroy()
  })

  test('synchronous early lifecycle publication failure retains failed compensation for scoped release', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => {
        throw transportError('unsubscribe')
      },
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    await expect(
      ipc.subscribeConnectionEvents(
        connection.handle,
        {
          connectionId: connection.connectionId,
          connectionGeneration: connection.connectionGeneration
        },
        undefined,
        () => connection.hasConfirmedRelease(),
        () => {
          throw new Error('publish failed')
        }
      )
    ).rejects.toBeInstanceOf(AggregateError)
    expect(inspectIpcProvisionalAdmissionForTests(ipc).unresolvedEventSubscriptionCount).toBe(1)
    expect(ipc.hasRegisteredStream('connection-events-ipc-2')).toBe(false)
    expect(commands.filter(command => command === 'connection.events.ready')).toHaveLength(1)
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(inspectIpcProvisionalAdmissionForTests(ipc).unresolvedEventSubscriptionCount).toBe(0)
    await ipc.destroy()
  })

  test.each(['success', 'refused'])(
    'held lifecycle ready remains owned through %s parent and late ready',
    async parent => {
      let ready
      const readyGate = new Promise(resolve => {
        ready = resolve
      })
      let attempts = 0
      const { ipc, connection, commands } = await createConnectedIpc({
        ready: () => readyGate,
        unsubscribe: () => new Promise(() => {}),
        disconnect: async () => ({
          kind: 'route',
          payload:
            ++attempts === 1 && parent === 'refused'
              ? {
                  state: 'release-failed',
                  failures: [{ resourceKind: 'connection', error: transportError('disconnect').normalized }]
                }
              : { state: 'released', failures: [] }
        })
      })
      expect(ipc.hasRegisteredStream('connection-events-ipc-1')).toBe(true)
      await expect(connection.release()).resolves.toMatchObject({
        state: parent === 'success' ? 'released' : 'release-failed'
      })
      expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
      expect(ipc.hasRegisteredStream('connection-events-ipc-1')).toBe(parent !== 'success')
      if (parent === 'refused') await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
      ready({ kind: 'route', payload: { state: 'ready' } })
      await new Promise(resolve => setImmediate(resolve))
      expect(ipc.hasRegisteredStream('connection-events-ipc-1')).toBe(false)
      await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
      expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
      await ipc.destroy()
    }
  )

  test.each(
    ['rejected', 'held', 'failed-receipt'].flatMap(child =>
      ['success', 'refused', 'rejected'].map(parent => [child, parent])
    )
  )('real GATT %s cleanup with %s parent keeps ownership until confirmation', async (child, parent) => {
    const failure = {
      state: 'release-failed',
      failures: [{ resourceKind: 'gatt', error: transportError('unsubscribe').normalized }]
    }
    const released = { kind: 'route', payload: { state: 'released', failures: [] } }
    let settleChild
    const held = new Promise(resolve => {
      settleChild = resolve
    })
    let attempts = 0
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => released,
      gattUnsubscribe: async () => {
        if (child === 'held') return held
        if (child === 'rejected') throw transportError('unsubscribe')
        return { kind: 'route', payload: failure }
      },
      disconnect: async () => {
        if (++attempts === 1 && parent === 'rejected') throw transportError('disconnect')
        return attempts === 1 && parent === 'refused' ? { kind: 'route', payload: failure } : released
      }
    })
    const database = await connection.discover()
    const subscription = await database.characteristics[0].subscribe()
    const release = connection.release()
    expect(() => database.assertCurrent()).toThrow()
    if (parent === 'rejected') await expect(release).rejects.toBeInstanceOf(AggregateError)
    else await expect(release).resolves.toMatchObject({ state: parent === 'success' ? 'released' : 'release-failed' })
    const other = new IpcConnection(ipc, 'other-handle', 'other-peer', 'other-id', 'other-lease', 'other-generation')
    await expect(other.readRssi()).resolves.toBe(-42)
    if (parent !== 'success') await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'released' })
    const removals = commands.filter(command => command === 'gatt.unsubscribe').length
    settleChild({ kind: 'route', payload: failure })
    await new Promise(resolve => setImmediate(resolve))
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'released' })
    expect(commands.filter(command => command === 'gatt.unsubscribe')).toHaveLength(removals)
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(parent === 'success' ? 1 : 2)
    await ipc.destroy()
  })

  test.each(['rejected', 'held', 'failed-receipt'])(
    '%s database cleanup cannot block scoped parent release',
    async outcome => {
      let complete
      const held = new Promise(resolve => {
        complete = resolve
      })
      const { ipc, connection, commands } = await createConnectedIpc({
        unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
        disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
      })
      const database = {
        invalidate: jest.fn(() =>
          outcome === 'held'
            ? held
            : outcome === 'rejected'
              ? Promise.reject(transportError('gatt.unsubscribe'))
              : Promise.resolve({ state: 'release-failed', failures: [] })
        ),
        confirmParentRelease: jest.fn()
      }
      connection.registerDatabase(database)
      const result = await connection.release()
      expect(commands).toContain('connection.disconnect')
      expect(result.state).toBe('released')
      expect(database.confirmParentRelease).toHaveBeenCalledTimes(1)
      await expect(connection.readRssi()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
      complete({ state: 'released', failures: [] })
      await connection.release()
      expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
      await ipc.destroy()
    }
  )

  test.each(['timeout', 'abort'])('discovery %s covers held old-database cleanup', async kind => {
    let complete
    const held = new Promise(resolve => {
      complete = resolve
    })
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const database = { invalidate: jest.fn(() => held), confirmParentRelease: jest.fn() }
    connection.registerDatabase(database)
    const abort = new AbortController()
    const discovery = connection.discover(kind === 'timeout' ? { timeoutMs: 5 } : { signal: abort.signal })
    if (kind === 'abort') abort.abort()
    await expect(discovery).rejects.toMatchObject({
      normalized: { code: kind === 'timeout' ? 'operation.timed-out' : 'operation.aborted' }
    })
    expect(commands).not.toContain('gatt.discover')
    complete({ state: 'released', failures: [] })
    await connection.release()
    await ipc.destroy()
  })

  test.each(['rejected', 'held', 'failed-receipt'])(
    '%s child cleanup stays owned when parent refuses then succeeds',
    async outcome => {
      const failed = {
        state: 'release-failed',
        failures: [{ resourceKind: 'connection', error: transportError('release').normalized }]
      }
      let complete
      const held = new Promise(resolve => {
        complete = resolve
      })
      let attempts = 0
      const { ipc, connection, commands } = await createConnectedIpc({
        unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
        disconnect: async () => ({
          kind: 'route',
          payload: ++attempts === 1 ? failed : { state: 'released', failures: [] }
        })
      })
      const database = {
        invalidate: jest.fn(() =>
          outcome === 'held'
            ? held
            : outcome === 'rejected'
              ? Promise.reject(transportError('gatt.unsubscribe'))
              : Promise.resolve(failed)
        ),
        confirmParentRelease: jest.fn()
      }
      connection.registerDatabase(database)
      await expect(connection.release()).resolves.toMatchObject({ state: 'release-failed' })
      expect(database.confirmParentRelease).not.toHaveBeenCalled()
      await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
      expect(database.invalidate.mock.calls.length).toBeGreaterThanOrEqual(2)
      expect(database.confirmParentRelease).toHaveBeenCalledTimes(1)
      expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(2)
      complete({ state: 'released', failures: [] })
      await ipc.destroy()
    }
  )

  test('held lifecycle unsubscribe cannot block connection release or reopen admission', async () => {
    let complete
    const held = new Promise(resolve => {
      complete = resolve
    })
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: () => held,
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const release = connection.release()
    await expect(connection.effectiveMtu()).rejects.toMatchObject({ normalized: { code: 'lifecycle.invalid-state' } })
    await expect(release).resolves.toMatchObject({ state: 'released' })
    expect(commands).toContain('connection.disconnect')
    complete({ kind: 'route', payload: { state: 'released', failures: [] } })
    await connection.release()
    expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
    await ipc.destroy()
  })

  test('held lifecycle native cleanup does not erase failed local cleanup after parent confirmation', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: () => new Promise(() => {}),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const close = ipc.closeStream.bind(ipc)
    let fail = true
    const localClose = jest.spyOn(ipc, 'closeStream').mockImplementation((...args) => {
      if (fail) throw new Error('local lifecycle close failed')
      return close(...args)
    })
    await expect(connection.release()).resolves.toMatchObject({ state: 'release-failed' })
    await new Promise(resolve => setImmediate(resolve))
    await expect(connection.release()).resolves.toMatchObject({ state: 'release-failed' })
    fail = false
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
    expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
    localClose.mockRestore()
    await ipc.destroy()
  })

  test('confirmed parent does not conceal local cleanup failure and retries only local cleanup', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const database = {
      invalidate: jest.fn(async () => ({ state: 'release-failed', failures: [] })),
      confirmParentRelease: jest.fn().mockImplementationOnce(() => {
        throw new Error('local close failed')
      })
    }
    connection.registerDatabase(database)
    await expect(connection.release()).resolves.toMatchObject({ state: 'release-failed' })
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    expect(database.confirmParentRelease).toHaveBeenCalledTimes(2)
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
    await ipc.destroy()
  })

  test.each(['rejected', 'held', 'failed-receipt'])(
    '%s child and rejected parent retain retry ownership without affecting another connection',
    async outcome => {
      const failed = {
        state: 'release-failed',
        failures: [{ resourceKind: 'connection', error: transportError('release').normalized }]
      }
      let complete
      const held = new Promise(resolve => {
        complete = resolve
      })
      let attempts = 0
      const { ipc, connection } = await createConnectedIpc({
        unsubscribe: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } }),
        disconnect: async () => {
          if (++attempts === 1) throw transportError('parent release')
          return { kind: 'route', payload: { state: 'released', failures: [] } }
        }
      })
      const other = new IpcConnection(ipc, 'other-handle', 'other-peer', 'other-id', 'other-lease', 'other-generation')
      const database = {
        invalidate: jest.fn(() =>
          outcome === 'held'
            ? held
            : outcome === 'rejected'
              ? Promise.reject(transportError('gatt.unsubscribe'))
              : Promise.resolve(failed)
        ),
        confirmParentRelease: jest.fn()
      }
      connection.registerDatabase(database)
      await expect(connection.release()).rejects.toBeInstanceOf(AggregateError)
      expect(database.confirmParentRelease).not.toHaveBeenCalled()
      await expect(other.readRssi()).resolves.toBe(-42)
      await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
      expect(database.confirmParentRelease).toHaveBeenCalledTimes(1)
      complete({ state: 'released', failures: [] })
      await ipc.destroy()
    }
  )

  test('unsubscribe rejection does not suppress connection.disconnect', async () => {
    const { ipc, connection, commands } = await createConnectedIpc({
      unsubscribe: async () => {
        throw transportError('ipc-manager.connection-events-unsubscribe')
      },
      disconnect: async () => ({ kind: 'route', payload: { state: 'released', failures: [] } })
    })
    const result = await connection.release()
    expect(commands).toEqual(expect.arrayContaining(['connection.events.unsubscribe', 'connection.disconnect']))
    expect(commands.indexOf('connection.disconnect')).toBeGreaterThan(commands.indexOf('connection.events.unsubscribe'))
    expect(result.state).toBe('released')
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
    expect(result.state).toBe('released')
    const before = commands.filter(command => command === 'connection.disconnect').length
    const retry = await connection.release()
    expect(retry.state).toBe('released')
    expect(commands.filter(command => command === 'connection.disconnect').length).toBe(before)
    expect(commands.filter(command => command === 'connection.events.unsubscribe')).toHaveLength(1)
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

  test('late subscribe success is already covered by confirmed parent release and cannot resurrect the connection', async () => {
    const { ipc, connection, commands, releaseSubscribe } = await createHungAdmission()
    await connection.release()
    expect(commands).toContain('connection.disconnect')
    expect(commands).not.toContain('connection.events.unsubscribe')
    releaseSubscribe()
    await flushMicrotasks()
    expect(commands).not.toContain('connection.events.unsubscribe')
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
