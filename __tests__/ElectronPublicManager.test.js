const { createElectronRendererBleManager } = require('../src/electron-renderer')
const { BleError } = require('../src/public/errors')
const { bootstrap } = require('./electron/helpers/public-bootstrap')

describe('Electron public manager façade', () => {
  test('projects unsupported security capabilities when IPC has no remote security backend', async () => {
    const current = bootstrap()
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      throw new Error(`unexpected routed request ${request.kind}`)
    })
    const manager = await createElectronRendererBleManager({
      transport: { invoke, subscribe: () => () => undefined, acknowledge: async () => ({ kind: 'event.ack' }) }
    })

    expect(manager.capabilities.get('security:state')).toMatchObject({ state: 'unsupported' })
    await expect(manager.security.state({ id: 'peer-1' })).resolves.toMatchObject({ bond: 'unsupported' })
  })

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
    // A host answer that does not say what the value is cannot back a receipt:
    // the renderer refuses it instead of assuming a read response.
    await expect(database.characteristic('180d', '2a37').readReceipt({ timeoutMs: 1_000 })).rejects.toMatchObject({
      code: 'protocol.malformed'
    })
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

  test('still routes connection.disconnect when lifecycle unsubscribe rejects', async () => {
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
            connectionId: 'connection-id-1',
            ownerLeaseId: current.rendererLease.leaseId,
            peerId: 'peer-1',
            connectionGeneration: 'connection-generation-1'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: request.envelope.payload.connectionEventsHandle,
            connectionId: request.envelope.payload.connectionId,
            connectionGeneration: request.envelope.payload.connectionGeneration,
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.events.unsubscribe') {
        return {
          kind: 'failure',
          error: {
            code: 'platform.transport',
            domain: 'connection',
            operation: 'electron.connection-events-unsubscribe',
            platform: null,
            retryability: 'caller-decides'
          }
        }
      }
      if (command === 'connection.disconnect') return { kind: 'route', payload: { state: 'released', failures: [] } }
      throw new Error(`unexpected command ${command}`)
    })
    const manager = await createElectronRendererBleManager({
      transport: {
        invoke,
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    const connection = await manager.connect('peer-1', { timeoutMs: 1_000 })
    void connection.events
    for (let attempt = 0; attempt < 30 && !commands.includes('connection.events.ready'); attempt += 1) {
      await new Promise(resolve => setImmediate(resolve))
    }
    const result = await connection.release()
    expect(commands).toEqual(expect.arrayContaining(['connection.events.unsubscribe', 'connection.disconnect']))
    expect(result).toEqual({ state: 'released', failures: [] })
    await expect(connection.release()).resolves.toEqual(result)
    expect(commands.filter(command => command === 'connection.disconnect')).toHaveLength(1)
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('invalidates the prior renderer database before explicit rediscovery', async () => {
    const current = bootstrap()
    const commands = []
    let discoveryCount = 0
    let readCount = 0
    const databasePayload = generation => ({
      schemaVersion: 2,
      handle: `database-${generation}`,
      databaseId: `database-id-${generation}`,
      databaseGeneration: `database-generation-${generation}`,
      services: [
        { uuid: '0000180d-0000-1000-8000-00805f9b34fb', occurrence: '0', primary: true, includedServices: [] }
      ],
      characteristics: [
        {
          handle: `characteristic-${generation}`,
          serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
          serviceOccurrence: '0',
          characteristicUuid: '00002a37-0000-1000-8000-00805f9b34fb',
          characteristicOccurrence: '0',
          properties: ['read']
        }
      ],
      descriptors: []
    })
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const command = request.envelope.command
      commands.push(command)
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-rediscovery',
            peerId: 'peer-rediscovery',
            connectionId: 'connection-id-rediscovery',
            ownerLeaseId: 'renderer-lease-1',
            connectionGeneration: 'connection-generation-rediscovery'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: request.envelope.payload.connectionEventsHandle,
            connectionId: 'connection-id-rediscovery',
            connectionGeneration: 'connection-generation-rediscovery',
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'gatt.discover') {
        discoveryCount += 1
        const payload = databasePayload(discoveryCount)
        return {
          kind: 'route',
          payload:
            request.envelope.payload.rediscoveryReason === undefined
              ? payload
              : { ...payload, rediscoveryReason: request.envelope.payload.rediscoveryReason }
        }
      }
      if (command === 'gatt.read') {
        readCount += 1
        return { kind: 'route', payload: { value: new Uint8Array([readCount]) } }
      }
      if (command === 'connection.events.unsubscribe') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      if (command === 'connection.disconnect') return { kind: 'route', payload: { state: 'released', failures: [] } }
      throw new Error(`unexpected command ${command}`)
    })
    const transport = {
      invoke,
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }

    const manager = await createElectronRendererBleManager({ transport })
    const connection = await manager.connect('peer-rediscovery', { timeoutMs: 1_000 })
    const first = await connection.discover({ timeoutMs: 1_000 })
    await expect(first.characteristic('180d', '2a37').read({ timeoutMs: 1_000 })).resolves.toEqual(new Uint8Array([1]))

    const second = await connection.rediscoverGatt({ reason: 'manual', timeoutMs: 1_000 })
    await expect(second.characteristic('180d', '2a37').read({ timeoutMs: 1_000 })).resolves.toEqual(new Uint8Array([2]))
    await expect(first.characteristic('180d', '2a37').read({ timeoutMs: 1_000 })).rejects.toMatchObject({
      code: 'gatt.stale-handle'
    })
    expect(readCount).toBe(2)
    expect(commands.filter(command => command === 'gatt.discover')).toHaveLength(2)
  })

  test('forwards the public operation signal through the nested Electron transport', async () => {
    let observedSignal
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.envelope.command === 'connection.connect') {
        observedSignal = request.signal
        return {
          kind: 'route',
          payload: {
            handle: 'connection-signal',
            peerId: 'peer-signal',
            connectionId: 'connection-id-signal',
            ownerLeaseId: 'renderer-lease-1',
            connectionGeneration: 'connection-generation-signal'
          }
        }
      }
      if (request.envelope.command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: request.envelope.payload.connectionEventsHandle,
            connectionId: 'connection-id-signal',
            connectionGeneration: 'connection-generation-signal',
            eventSchemaVersion: 2
          }
        }
      }
      if (request.envelope.command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (request.envelope.command === 'connection.events.unsubscribe') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      throw new Error(`unexpected command ${request.envelope.command}`)
    })
    const transport = {
      invoke,
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    }
    const manager = await createElectronRendererBleManager({ transport })
    const signal = new AbortController().signal

    await manager.connect('peer-signal', { signal })
    expect(observedSignal).toBe(signal)
  })

  test('use after destroy rejects with a public lifecycle BleError without Tauri wording', async () => {
    const current = bootstrap()
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: {} }
    })
    const manager = await createElectronRendererBleManager({
      transport: { invoke, subscribe: () => () => undefined, acknowledge: async () => ({ kind: 'event.ack' }) }
    })
    await manager.destroy()

    await expect(manager.adapter.state()).rejects.toMatchObject({
      name: 'BleError',
      code: 'lifecycle.destroyed',
      domain: 'ipc'
    })
    await expect(manager.scan()).rejects.toMatchObject({
      name: 'BleError',
      code: 'lifecycle.destroyed',
      domain: 'ipc'
    })
    await expect(manager.connect('peer-1')).rejects.toMatchObject({
      name: 'BleError',
      code: 'lifecycle.destroyed',
      domain: 'ipc'
    })
    await manager.adapter.state().catch(error => {
      expect(String(error)).not.toMatch(/Tauri/i)
      expect(error).not.toBeInstanceOf(TypeError)
    })
  })
})
