'use strict'

class FakeChannel {
  constructor() {
    this.onmessage = null
    FakeChannel.current = this
  }

  emit(message) {
    this.onmessage?.(message)
  }
}

function negotiated(axis) {
  const selected = { axis, value: 1 }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function bootstrap() {
  const backendGeneration = 'backend-generation-1'
  const attachment = {
    attachmentId: 'tauri-attachment-1',
    backendInstanceId: 'tauri-btleplug-1',
    backendGeneration,
    adapter: {
      adapterId: 'tauri-adapter-1',
      displayName: 'Bluetooth',
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        backendGeneration,
        updatedAt: 1,
        safeReason: null
      },
      adapterGeneration: 'adapter-generation-1',
      limitations: []
    }
  }
  return {
    attachment,
    attachmentId: attachment.attachmentId,
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    renderer: {
      clientId: 'tauri-client-1',
      windowScope: 'main',
      sessionScope: 'session-1'
    },
    rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' }
  }
}

function streamValue(streamId, value, eventId = `${streamId}-event`) {
  FakeChannel.current.emit({
    rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' },
    eventId,
    streamId,
    item: { kind: 'value', value }
  })
}

function advertisement(peerId, localName, rssi) {
  return {
    peerId,
    localName,
    rssi,
    txPowerLevel: null,
    serviceUuids: [],
    manufacturerData: [],
    serviceData: []
  }
}

describe('Tauri v2 public manager', () => {
  test('runs scan, connect, GATT, notifications, and deterministic cleanup through one manager surface', async () => {
    const commands = []
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }

      const { command, payload } = request.envelope
      commands.push(command)
      const responses = {
        'adapter.state': { state: bootstrap().attachment.adapter.state },
        'scan.start': { handle: 'scan-1' },
        'scan.stop': { state: 'released', failures: [] },
        'connection.connect': {
          handle: 'connection-1',
          connectionId: 'connection-id-1',
          ownerLeaseId: 'tauri-lease-1',
          peerId: 'polar-h10',
          connectionGeneration: 'generation-1'
        },
        'gatt.discover': {
          handle: 'database-1',
          databaseGeneration: 'database-generation-1',
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
        },
        'gatt.read': { value: { $__unifiedBleBytesV2: [1, 2, 3] } },
        'gatt.write': {
          terminal: { correlation: 'write-operation-1', outcome: 'succeeded', cause: null },
          mode: 'with-response',
          commitState: 'confirmed',
          bytesSubmitted: 2
        },
        'gatt.subscribe': { handle: 'subscription-1' },
        'gatt.unsubscribe': { state: 'released', failures: [] },
        'connection.disconnect': { state: 'released', failures: [] }
      }
      return { kind: 'route', payload: responses[command] ?? { accepted: true, payload } }
    })

    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(manager.adapterState()).resolves.toMatchObject({ power: 'on' })
    const scan = await manager.scan({
      filter: { serviceUuids: ['180d'], manufacturerData: [], localNamePrefix: null }
    })
    const observation = scan.observations[Symbol.asyncIterator]().next()
    streamValue('scan-1', advertisement('polar-h10', 'Polar H10', -47))
    await expect(observation).resolves.toMatchObject({ value: { kind: 'value', value: { peerId: 'polar-h10' } } })
    await expect(scan.stop()).resolves.toMatchObject({ state: 'released' })

    const connection = await manager.connect('polar-h10')
    expect(connection.connectionId).toBe('connection-id-1')
    const database = await connection.discover()
    expect(database.path.databaseGeneration).toBe('database-generation-1')
    expect(database.path.attachment.attachmentId).toBe('tauri-attachment-1')
    expect(database.path.attachment.adapter.adapterId).toBe('tauri-adapter-1')
    const characteristic = database.characteristics[0]
    await expect(characteristic.read()).resolves.toEqual(new Uint8Array([1, 2, 3]))
    await expect(characteristic.write(new Uint8Array([4, 5]), { mode: 'with-response' })).resolves.toMatchObject({
      terminal: { correlation: 'write-operation-1', outcome: 'succeeded', cause: null },
      mode: 'with-response',
      commitState: 'confirmed',
      bytesSubmitted: 2
    })
    const snapshot = await database.snapshot()
    await expect(
      database.write(snapshot.characteristics[0].path, new Uint8Array([4, 5]), { mode: 'with-response' })
    ).resolves.toMatchObject({
      terminal: { correlation: 'write-operation-1', outcome: 'succeeded', cause: null },
      mode: 'with-response',
      commitState: 'confirmed',
      bytesSubmitted: 2
    })

    const subscription = await characteristic.subscribe()
    const notification = subscription.values[Symbol.asyncIterator]().next()
    streamValue('subscription-1', {
      value: new Uint8Array([6, 7]),
      delivery: 'unknown',
      observedAtMonotonicMs: 1,
      sequence: 1
    })
    await expect(notification).resolves.toMatchObject({
      value: {
        kind: 'value',
        value: { value: new Uint8Array([6, 7]), delivery: 'unknown', observedAtMonotonicMs: 1, sequence: 1 }
      }
    })
    await expect(subscription.remove()).resolves.toMatchObject({ state: 'released' })
    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'released' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })

    expect(commands).toEqual([
      'adapter.state',
      'scan.start',
      'scan.stop',
      'connection.connect',
      'gatt.discover',
      'gatt.read',
      'gatt.write',
      'gatt.write',
      'gatt.subscribe',
      'gatt.unsubscribe',
      'connection.disconnect'
    ])
  })

  test('rehydrates host failures as public BleError values', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return {
        kind: 'failure',
        error: {
          code: 'connection.failed',
          domain: 'connection',
          operation: 'tauri.connect',
          platform: null,
          retryability: 'never'
        }
      }
    })
    const { BleError } = require('../src')
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(manager.connect('polar-h10')).rejects.toMatchObject({
      constructor: BleError,
      code: 'connection.failed',
      operation: 'tauri.connect',
      recovery: { actions: [{ kind: 'reconnect' }] }
    })
    await manager.destroy()
  })

  test('rehydrates cleanup failures from public connection resources', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.envelope.command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-cleanup',
            connectionId: 'connection-id-cleanup',
            ownerLeaseId: 'tauri-lease-1',
            peerId: 'polar-h10',
            connectionGeneration: 'generation-cleanup'
          }
        }
      }
      if (request.envelope.command === 'connection.disconnect') {
        return {
          kind: 'failure',
          error: {
            code: 'connection.lost',
            domain: 'connection',
            operation: 'tauri.disconnect',
            platform: null,
            retryability: 'never'
          }
        }
      }
      return { kind: 'route', payload: {} }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const connection = await manager.connect('polar-h10')

    await expect(connection.release()).rejects.toMatchObject({ code: 'connection.lost' })
    await manager.destroy()
  })

  test('propagates AbortSignal cancellation through the shared IPC client', async () => {
    let resolveConnect
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.envelope.command === 'operation.cancel') {
        resolveConnect({
          kind: 'failure',
          error: {
            code: 'operation.aborted',
            domain: 'ipc',
            operation: 'tauri.connect',
            platform: null,
            retryability: 'caller-decides'
          }
        })
        return { kind: 'route', payload: { state: 'cancellation-requested' } }
      }
      return new Promise(resolve => {
        resolveConnect = resolve
      })
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const controller = new AbortController()
    const connecting = manager.connect('polar-h10', { signal: controller.signal })

    await Promise.resolve()
    await Promise.resolve()
    controller.abort()

    await expect(connecting).rejects.toMatchObject({ code: 'operation.aborted' })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'operation.cancel')).toBe(true)
    await manager.destroy()
  })

  test('turns a manager timeout into native operation cancellation', async () => {
    let resolveConnect
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.envelope.command === 'operation.cancel') {
        resolveConnect({
          kind: 'failure',
          error: {
            code: 'operation.aborted',
            domain: 'ipc',
            operation: 'tauri.connect',
            platform: null,
            retryability: 'caller-decides'
          }
        })
        return { kind: 'route', payload: { state: 'cancellation-requested' } }
      }
      return new Promise(resolve => {
        resolveConnect = resolve
      })
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(manager.connect('polar-h10', { timeoutMs: 1 })).rejects.toMatchObject({ code: 'operation.timed-out' })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'operation.cancel')).toBe(true)
    await manager.destroy()
  })

  test('allows destroy to be retried after a transient transport rejection', async () => {
    let releases = 0
    const releaseError = new Error('transport unavailable')
    const invoke = jest.fn(async (_command, args) => {
      if (args.request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (args.request.kind === 'release') {
        releases += 1
        if (releases === 1) throw releaseError
        return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      }
      if (args.request.kind === 'event.ack') return { kind: 'event.ack' }
      return { kind: 'route', payload: { state: bootstrap().attachment.adapter.state } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(manager.destroy()).rejects.toThrow('transport unavailable')
    expectConsoleError('[ElectronRendererBleClient] Release failed; client remains retryable:', releaseError)
    await expect(manager.adapterState()).resolves.toMatchObject({ power: 'on' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('rehydrates compatibility adapter-state failures as public errors', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return {
        kind: 'failure',
        error: {
          code: 'adapter.unavailable',
          domain: 'adapter',
          operation: 'tauri.adapter-state',
          platform: null,
          retryability: 'never'
        }
      }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(manager.adapterState()).rejects.toMatchObject({ code: 'adapter.unavailable' })
    await manager.destroy()
  })

  test('delivers scan advertisements that arrived before the renderer registered the stream', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.envelope.command === 'scan.start') {
        streamValue('scan-1', advertisement('early-peer', 'Early', -40))
        return { kind: 'route', payload: { handle: 'scan-1' } }
      }
      if (request.envelope.command === 'scan.stop') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      return { kind: 'route', payload: { accepted: true } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const scan = await manager.scan()
    const first = await scan.observations[Symbol.asyncIterator]().next()
    expect(first.done).toBe(false)
    expect(first.value).toMatchObject({ kind: 'value', value: { peerId: 'early-peer' } })
    await scan.stop()
    await manager.destroy()
  })

  test('rejects bootstrap records with contradictory identity or adapter state', async () => {
    const invalidBootstrap = bootstrap()
    invalidBootstrap.attachmentId = 'mismatched-attachment'
    const invoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: invalidBootstrap }))
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')

    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toMatchObject({
      normalized: { code: 'protocol.malformed' }
    })

    const invalidStateBootstrap = bootstrap()
    invalidStateBootstrap.attachment.adapter.state.power = 'not-a-power-state'
    const invalidStateInvoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: invalidStateBootstrap }))

    await expect(
      createTauriBleManagerWithEnvironment({ invoke: invalidStateInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })

    const mismatchedGenerationBootstrap = bootstrap()
    mismatchedGenerationBootstrap.attachment.adapter.state.backendGeneration = 'other-generation'
    const mismatchedGenerationInvoke = jest.fn(async () => ({
      kind: 'bootstrap',
      bootstrap: mismatchedGenerationBootstrap
    }))
    await expect(
      createTauriBleManagerWithEnvironment({ invoke: mismatchedGenerationInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })

    const outOfRangeVersionBootstrap = bootstrap()
    outOfRangeVersionBootstrap.versions.ipcProtocol.selected = { axis: 'ipc-protocol', value: 2 }
    const outOfRangeVersionInvoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: outOfRangeVersionBootstrap }))
    await expect(
      createTauriBleManagerWithEnvironment({ invoke: outOfRangeVersionInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
  })

  test('rejects malformed public filters instead of silently scanning without them', async () => {
    const invoke = jest.fn(async (_command, args) => {
      if (args.request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (args.request.kind === 'event.ack') return { kind: 'event.ack' }
      if (args.request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: { handle: 'scan-should-not-start' } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    for (const filter of [{ serviceUuids: 'not-an-array' }, [], new Date()]) {
      await expect(manager.scan({ filter })).rejects.toMatchObject({ code: 'argument.invalid' })
    }
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'scan.start')).toBe(false)
    await manager.destroy()
  })
})
