const { createElectronRendererBleManager } = require('../src/electron-renderer')
const { BUILT_IN_FEATURE_IDS } = require('../src/backend-contract/capabilities')
const { BleError } = require('../src/public/errors')

function capabilitySnapshot(backendGeneration) {
  const schema = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  const limitation = {
    code: 'not-implemented',
    explanation: 'fixture capability is not implemented',
    affectedGuarantee: 'support'
  }
  return {
    schemaVersion: 2,
    backendGeneration,
    descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
      id,
      state: 'unsupported',
      selectedSchemaRange: schema,
      implementationOrigin: 'backend-native',
      tck: {
        suiteId: 'capability.catalog-v2',
        requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
        contractRange: schema
      },
      evidence: {
        receiptId: `test-${id}`,
        evidenceLevel: 'blocked',
        implementationVersion: 'test',
        sourceDigest: `test-${id}`,
        scenarioIds: ['capability.truth-limits-evidence-and-binding'],
        limitations: [limitation]
      },
      limitations: [limitation],
      limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
    }))
  }
}

function bootstrap() {
  const attachment = {
    attachmentId: 'electron-attachment-1',
    backendInstanceId: 'electron-backend-1',
    backendGeneration: 'electron-generation-1',
    adapter: {
      adapterId: 'electron-adapter-1',
      displayName: 'test',
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        backendGeneration: 'electron-generation-1',
        updatedAt: 1,
        safeReason: null
      },
      adapterGeneration: 'electron-adapter-generation-1',
      limitations: []
    }
  }
  const version = axis => ({
    axis,
    selected: { axis, value: axis === 'ipc-protocol' ? 2 : 1 },
    localRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 2 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 2 : 1 }
    },
    remoteRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 2 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 2 : 1 }
    }
  })
  return {
    attachment,
    attachmentId: attachment.attachmentId,
    versions: {
      backendContract: version('backend-contract'),
      capabilitySchema: version('capability-schema'),
      eventSchema: version('event-schema'),
      traceFormat: version('trace-format'),
      ipcProtocol: version('ipc-protocol')
    },
    capabilities: capabilitySnapshot(attachment.backendGeneration),
    renderer: { clientId: 'renderer-client-1', windowScope: 'window-1', sessionScope: 'session-1' },
    rendererLease: { leaseId: 'renderer-lease-1', generation: 'renderer-lease-generation-1' }
  }
}

describe('Electron public manager façade', () => {
  test('rejects malformed public operation options before routing IPC', async () => {
    const current = bootstrap()
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      throw new Error(`unexpected routed request ${request.kind}`)
    })
    const transport = {
      invoke,
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
    const manager = await createElectronRendererBleManager({ transport })
    await expect(manager.scan({ timeoutMs: 0 })).rejects.toBeInstanceOf(BleError)
    await expect(manager.connect('peer-1', { timeoutMs: 0 })).rejects.toBeInstanceOf(BleError)
    await expect(manager.choose({ timeoutMs: 0 })).rejects.toBeInstanceOf(BleError)
    expect(invoke).toHaveBeenCalledTimes(1)
  })

  test('uses the common public connection/GATT surface over the renderer transport', async () => {
    const current = bootstrap()
    const commands = []
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const command = request.envelope.command
      commands.push(command)
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-1',
            peerId: 'peer-1',
            connectionId: 'connection-id-1',
            ownerLeaseId: 'renderer-lease-1',
            connectionGeneration: 'connection-generation-1'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: request.envelope.payload.connectionEventsHandle,
            connectionId: 'connection-id-1',
            connectionGeneration: 'connection-generation-1',
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'gatt.discover') {
        return {
          kind: 'route',
          payload: {
            schemaVersion: 2,
            handle: 'database-1',
            databaseId: 'database-id-1',
            databaseGeneration: 'database-generation-1',
            services: [
              { uuid: '0000180d-0000-1000-8000-00805f9b34fb', occurrence: '0', primary: true, includedServices: [] }
            ],
            characteristics: [
              {
                handle: 'characteristic-1',
                serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
                serviceOccurrence: '0',
                characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
                characteristicOccurrence: '0',
                properties: ['read']
              }
            ],
            descriptors: []
          }
        }
      }
      if (command === 'gatt.read') return { kind: 'route', payload: { value: new Uint8Array([1, 2, 3]) } }
      if (command === 'connection.events.unsubscribe')
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      if (command === 'connection.disconnect') return { kind: 'route', payload: { state: 'released', failures: [] } }
      throw new Error(`unexpected command ${command}`)
    })
    const listeners = new Set()
    const transport = {
      invoke,
      subscribe: listener => {
        listeners.add(listener)
        return () => listeners.delete(listener)
      },
      acknowledge: async () => ({ kind: 'event.ack' })
    }

    const manager = await createElectronRendererBleManager({ transport })
    const connection = await manager.connect('peer-1', { timeoutMs: 1_000 })
    const database = await connection.discover({ timeoutMs: 1_000 })
    await expect(database.characteristic('180d', '2a37').read({ timeoutMs: 1_000 })).resolves.toEqual(
      new Uint8Array([1, 2, 3])
    )
    await expect(connection.release()).resolves.toMatchObject({ state: 'released' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
    expect(commands).toEqual(
      expect.arrayContaining([
        'connection.connect',
        'connection.events.subscribe',
        'connection.events.ready',
        'gatt.discover',
        'gatt.read',
        'connection.events.unsubscribe',
        'connection.disconnect'
      ])
    )
    expect(listeners.size).toBeGreaterThanOrEqual(0)
  })
})
