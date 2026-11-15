const { createElectronRendererBleManager } = require('../src/electron-renderer')
const { BleError } = require('../src/public/errors')
const { bootstrap } = require('./electron/helpers/public-bootstrap')
const { contractError } = require('../src/backend-contract/errors')

describe('IPC direct peer-reference connection', () => {
  const reference = { version: 1, backendId: 'corebluetooth', scope: 'system', opaqueId: 'os-guid' }
  const peer = {
    peerId: 'os-guid',
    reference,
    name: null,
    rssi: null,
    source: 'app-reference',
    state: { reachability: 'unknown', connection: 'unknown', bond: 'unknown', lastSeenAtMonotonicMs: null }
  }
  async function fixture(resolve) {
    const calls = []
    const manager = await createElectronRendererBleManager({
      transport: {
        invoke: async request => {
          if (request.kind === 'bootstrap') {
            const value = bootstrap()
            const selected = { axis: 'ipc-protocol', value: 11 }
            value.versions.ipcProtocol = {
              axis: 'ipc-protocol', selected,
              localRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected },
              remoteRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected }
            }
            return { kind: 'bootstrap', bootstrap: value }
          }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          calls.push(request.envelope)
          if (request.envelope.command === 'peers.resolve') return { kind: 'route', payload: { peer: await resolve() } }
          if (request.envelope.command === 'operation.cancel')
            return { kind: 'route', payload: { state: 'cancellation-requested' } }
          if (request.envelope.command === 'connection.connect')
            throw contractError('connection.failed', 'connection', 'test.connect-observed')
          throw Error(`unexpected ${request.envelope.command}`)
        },
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    return { manager, calls }
  }
  test('resolves before connect and spends one original deadline', async () => {
    jest.useFakeTimers()
    try {
      let settle
      const { manager, calls } = await fixture(
        () =>
          new Promise(resolve => {
            settle = resolve
          })
      )
      const result = expect(manager.connect(reference, { timeoutMs: 100 })).rejects.toMatchObject({
        code: 'connection.failed',
        operation: 'test.connect-observed'
      })
      await jest.advanceTimersByTimeAsync(40)
      settle(peer)
      await result
      const connect = calls.find(call => call.command === 'connection.connect')
      expect(connect.payload.peerId).toBe('os-guid')
      expect(connect.payload.budgetMs).toBeLessThanOrEqual(60)
      expect(connect.payload.budgetMs).toBeGreaterThan(0)
      await manager.destroy()
    } finally {
      jest.useRealTimers()
    }
  })
  test.each(['missing', 'wrong-scope', 'abort', 'deadline'])('%s resolution cannot admit a connect', async kind => {
    jest.useFakeTimers()
    try {
      const controller = new AbortController()
      const { manager, calls } = await fixture(async () => {
        if (kind === 'wrong-scope') throw contractError('peer.scope-mismatch', 'connection', 'test.resolve')
        if (kind === 'abort') controller.abort()
        if (kind === 'deadline') await new Promise(resolve => setTimeout(resolve, 30))
        return kind === 'missing' ? null : peer
      })
      const expected = {
        missing: 'peer.not-found',
        'wrong-scope': 'peer.scope-mismatch',
        abort: 'operation.aborted',
        deadline: 'operation.timed-out'
      }[kind]
      const result = expect(
        manager.connect(reference, { signal: controller.signal, timeoutMs: 20 })
      ).rejects.toMatchObject({ code: expected })
      await jest.advanceTimersByTimeAsync(40)
      await result
      expect(calls.some(call => call.command === 'connection.connect')).toBe(false)
      await manager.destroy()
    } finally {
      jest.useRealTimers()
    }
  })
})

describe('Electron public manager façade', () => {
  test('opts into terminal evidence through opaque IPC recovery handles', async () => {
    let acknowledged = false
    const manager = await createElectronRendererBleManager({
      terminalEvidence: { maximumCanonicalBytes: 3 },
      transport: {
        invoke: async request => {
          if (request.kind === 'bootstrap') {
            const value = bootstrap()
            const selected = { axis: 'ipc-protocol', value: 11 }
            value.versions.ipcProtocol = {
              axis: 'ipc-protocol', selected,
              localRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected },
              remoteRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected }
            }
            return { kind: 'bootstrap', bootstrap: value }
          }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          if (request.envelope.command === 'terminalEvidence.recovery.list') {
            expect(request.envelope.payload).toEqual({ version: 1 })
            return { kind: 'route', payload: acknowledged ? { records: [] } : { records: [{ handle: 'opaque', producer: 'scan', byteLength: 3, digest: 'a'.repeat(64), formatVersion: 1 }] } }
          }
          if (request.envelope.command === 'terminalEvidence.recovery.read') {
            expect(request.envelope.payload).toEqual({ version: 1, handle: 'opaque', maximumBytes: 3 })
            return { kind: 'route', payload: { kind: 'committed', canonicalBytes: new Uint8Array([1, 2, 3]), digest: 'a'.repeat(64) } }
          }
          if (request.envelope.command === 'terminalEvidence.recovery.ack') {
            expect(request.envelope.payload).toEqual({ version: 1, handle: 'opaque' })
            acknowledged = true
            return { kind: 'route', payload: { state: 'released' } }
          }
          throw new Error(`unexpected ${request.envelope.command}`)
        },
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    const [record] = await manager.terminalEvidenceRecovery.list()
    expect(record).toEqual({ handle: 'opaque', producer: 'scan', byteLength: 3, digest: 'a'.repeat(64), formatVersion: 1 })
    await expect(manager.terminalEvidenceRecovery.read(record.handle, 3)).resolves.toEqual({
      canonicalBytes: new Uint8Array([1, 2, 3]), digest: 'a'.repeat(64)
    })
    await manager.terminalEvidenceRecovery.acknowledge(record.handle)
    await expect(manager.terminalEvidenceRecovery.list()).resolves.toEqual([])
    await manager.destroy()
  })

  test('rejects duplicate busy producer lanes in terminal-evidence recovery status', async () => {
    const statusBootstrap = bootstrap()
    const selected = { axis: 'ipc-protocol', value: 11 }
    statusBootstrap.versions.ipcProtocol = {
      axis: 'ipc-protocol', selected,
      localRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected },
      remoteRange: { axis: 'ipc-protocol', minimum: selected, maximum: selected }
    }
    const manager = await createElectronRendererBleManager({
      terminalEvidence: { maximumCanonicalBytes: 3 },
      transport: {
        invoke: async request => {
          if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: statusBootstrap }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          if (request.envelope.command === 'terminalEvidence.recovery.listWithStatus') {
            return { kind: 'route', payload: { records: [], busyProducers: ['scan', 'scan'] } }
          }
          throw new Error(`unexpected ${request.envelope.command}`)
        },
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    await expect(manager.terminalEvidenceRecovery.listWithStatus()).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed', operation: 'electron.terminal-evidence-recovery.list-with-status' }
    })
    await manager.destroy()
  })

  test('fails closed on malformed raw recovery envelopes and owns canonical token bytes', async () => {
    const manager = await createElectronRendererBleManager({
      transport: {
        invoke: async request => {
          if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          if (request.envelope.command === 'scanFailureRecovery.raw.token')
            return { kind: 'route', payload: { token: { kind: 'bytes', total: '2', bytes: [1, 2] } } }
          if (request.envelope.command === 'scanFailureRecovery.raw.info')
            return { kind: 'route', payload: { info: { memberCount: 1, tokenCount: 1 } } }
          throw new Error(`unexpected ${request.envelope.command}`)
        },
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    const token = await manager.scanFailureRecovery.rawToken('recovery', 'owner', 0, 0, 2)
    expect(token).toEqual({ kind: 'bytes', total: '2', bytes: new Uint8Array([1, 2]) })
    expect(token.bytes).not.toBeInstanceOf(Array)
    await expect(manager.scanFailureRecovery.rawInfo('recovery', 'owner')).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })
    await expect(manager.scanFailureRecovery.rawToken('recovery', 'owner', 0, 0, 0)).rejects.toMatchObject({
      normalized: { code: 'argument.invalid' }
    })
    await expect(manager.scanFailureRecovery.rawToken('recovery', 'owner', 0, 0, 65537)).rejects.toMatchObject({
      normalized: { code: 'argument.invalid' }
    })
    await manager.destroy()
  })

  test('projects durable recovery through opaque handles without renderer provenance authority', async () => {
    const calls = []
    const manager = await createElectronRendererBleManager({
      transport: {
        invoke: async request => {
          if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
          if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
          calls.push(request.envelope)
          if (request.envelope.command === 'peerFailureRecovery.list')
            return {
              kind: 'route',
              payload: {
                recoveries: [
                  {
                    handleId: 'opaque-handle',
                    peerId: 'private-peer',
                    selector: {
                      serviceUuid: '180d',
                      serviceOccurrence: '0',
                      characteristicUuid: '2a37',
                      characteristicOccurrence: '0'
                    },
                    contiguousRead: '0',
                    acknowledged: false,
                    producerAck: 'not-started'
                  }
                ]
              }
            }
          if (request.envelope.command === 'peerFailureRecovery.read')
            return {
              kind: 'route',
              payload: { offset: '0', nextOffset: '2', bytes: new Uint8Array([1, 2]), done: true }
            }
          if (request.envelope.command === 'peerFailureRecovery.ack')
            return { kind: 'route', payload: { acknowledged: true } }
          throw new Error(`unexpected ${request.envelope.command}`)
        },
        subscribe: () => () => undefined,
        acknowledge: async () => ({ kind: 'event.ack' })
      }
    })
    const [recovery] = await manager.peerFailureRecovery.list()
    expect(recovery).toMatchObject({ handleId: 'opaque-handle', peerId: 'private-peer' })
    expect(recovery).not.toHaveProperty('consumer')
    expect(recovery).not.toHaveProperty('recipientLease')
    await manager.peerFailureRecovery.read(recovery.handleId, '0')
    await manager.peerFailureRecovery.acknowledge(recovery.handleId)
    expect(calls.map(call => call.payload)).toContainEqual({ handleId: 'opaque-handle', offset: '0' })
    expect(calls.map(call => call.payload)).toContainEqual({ handleId: 'opaque-handle' })
    await manager.destroy()
  })

  test('retains native security capabilities and routes its measured state', async () => {
    const current = bootstrap()
    const invoke = jest.fn(async request => {
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: current }
      if (request.envelope?.command === 'security.state')
        return {
          kind: 'route',
          payload: {
            state: {
              bond: 'not-bonded',
              encryption: 'unknown',
              authentication: 'unknown',
              secureConnections: 'unknown',
              pairingPossible: null,
              measuredAtMonotonicMs: 1,
              limitations: []
            }
          }
        }
      throw new Error(`unexpected routed request ${request.kind}`)
    })
    const manager = await createElectronRendererBleManager({
      transport: { invoke, subscribe: () => () => undefined, acknowledge: async () => ({ kind: 'event.ack' }) }
    })

    expect(manager.capabilities.get('security:state')).toMatchObject({ state: 'limited' })
    await expect(manager.security.state({ id: 'peer-1' })).resolves.toMatchObject({ bond: 'not-bonded' })
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

  test('protocol-6 peer supports the 5.2.1 connection/GATT route set and gates v10 recovery before dispatch', async () => {
    const current = bootstrap()
    const protocol6 = { axis: 'ipc-protocol', value: 6 }
    const protocol6Range = { axis: 'ipc-protocol', minimum: protocol6, maximum: protocol6 }
    current.versions = {
      ...current.versions,
      ipcProtocol: {
        axis: 'ipc-protocol',
        selected: protocol6,
        localRange: protocol6Range,
        remoteRange: protocol6Range
      }
    }
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
    await expect(manager.peerFailureRecovery.list()).rejects.toMatchObject({
      normalized: { code: 'capability.unsupported' }
    })
    expect(commands).not.toContain('peerFailureRecovery.list')
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
