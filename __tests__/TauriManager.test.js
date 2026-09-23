// __tests__/TauriManager.test.js

'use strict'

const { BUILT_IN_FEATURE_IDS } = require('../src/backend-contract/capabilities')
const { awaitSignal } = require('./helpers/async')

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
  const selected = { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function capabilityDescriptor(id, scenario, state = 'limited', limitationCode = null) {
  const limitation = {
    code: limitationCode ?? (state === 'limited' ? 'deterministic-only' : 'not-implemented'),
    explanation:
      state === 'limited'
        ? 'The fixture exposes deterministic host evidence only.'
        : 'The fixture does not implement this capability.',
    affectedGuarantee: state === 'limited' ? 'Physical-radio qualification is not claimed.' : 'support'
  }
  const schemaRange = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  return {
    id,
    state,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native',
    tck: { suiteId: 'capability.catalog-v2', requiredScenarioIds: [scenario], contractRange: schemaRange },
    evidence: {
      receiptId: `fixture-${id}`,
      evidenceLevel: state === 'limited' ? 'deterministic' : 'blocked',
      implementationVersion: 'fixture-v2',
      sourceDigest: `fixture-${id}`,
      scenarioIds: [scenario],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }
}

function capabilitySnapshot(
  backendGeneration,
  // Finding 217 follow-up: the fixture models the renegotiated native
  // snapshot per test — unsupported with the desktop reason (a platform
  // that genuinely cannot answer), or limited with the OS derivation.
  effectiveMtuEntry = [
    'connection:effective-mtu',
    'connection.rssi-and-att-mtu-capability-contract',
    'unsupported',
    'effective-mtu-boundary-unavailable'
  ]
) {
  const entries = [
    ['discovery:continuous-scan', 'scan.owner-join-authority-and-signature'],
    ['connection:direct', 'connection.lease-joins-borrowing-transfer-and-revocation'],
    ['connection:rssi', 'connection.rssi-and-att-mtu-capability-contract'],
    ['gatt:descriptors', 'gatt.descriptor-discovery-read-write'],
    ['gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation'],
    // Finding 190b (owner decision J): Tauri advertises max-write and
    // long-write like the desktop core over the same Rust core.
    ['gatt:maximum-write-length', 'gatt.maximum-write-length'],
    ['gatt:long-write', 'gatt.long-write'],
    effectiveMtuEntry
  ]
  const metadata = new Map(entries.map(([id, scenario, state = 'limited', limitationCode = null]) => [id, { scenario, state, limitationCode }]))
  return {
    schemaVersion: 2,
    backendGeneration,
    descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => {
      const meta = metadata.get(id)
      return capabilityDescriptor(
        id,
        meta?.scenario ?? 'capability.truth-limits-evidence-and-binding',
        meta?.state ?? 'unsupported',
        meta?.limitationCode ?? null
      )
    })
  }
}

function bootstrap(effectiveMtuEntry) {
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
        heard: null,
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
    capabilities: capabilitySnapshot(backendGeneration, effectiveMtuEntry),
    // F01: the 5.0 lane plugin always reports its linked shared-core
    // identity; fixtures simulate the lane plugin, not a legacy host.
    core: {
      contractRevision: 'C-UBM.0.1.2-DRAFT',
      implementationVersion: '5.0.0-rc.3'
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
  test('rejects reference connections explicitly when the Tauri directory is unsupported', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected route ${request.envelope.command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(
      manager.connect({ version: 1, backendId: 'unified-ble:tauri', scope: 'system', opaqueId: 'peer-1' })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'connection.connect')).toBe(false)
    await manager.destroy()
  })

  test('rejects malformed reference-shaped peers before IPC connect', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected route ${request.envelope.command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    await expect(
      manager.connect({ version: 2, backendId: 'unified-ble:tauri', scope: 'system', opaqueId: 'peer-1' })
    ).rejects.toMatchObject({ code: 'peer.reference-invalid' })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'connection.connect')).toBe(false)
    await manager.destroy()
  })

  test('rejects connection options that the Tauri IPC contract cannot route', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected route ${request.envelope.command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    // Finding 190b (owner decision J): advertised like the desktop core.
    expect(manager.capabilities.supports('gatt:maximum-write-length')).toBe(true)
    expect(manager.capabilities.supports('gatt:long-write')).toBe(true)
    await expect(manager.connect('peer-1', { intent: 'when-available' })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    await expect(manager.connect('peer-1', { preferredPhy: ['le-2m'] })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    await expect(manager.connect('peer-1', { transport: 'le' })).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'connection.connect')).toBe(false)
    await manager.destroy()
  })

  test('finding 190b: maximum-write-length is measured through IPC while an unreported effective MTU keeps the desktop reason', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command, payload } = request.envelope
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-1',
            connectionId: 'connection-id-1',
            ownerLeaseId: 'tauri-lease-1',
            peerId: 'polar-h10',
            connectionGeneration: 'generation-1'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-events-ipc-1',
            connectionId: 'connection-id-1',
            connectionGeneration: 'generation-1',
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.maximum-write-length') {
        expect(payload.mode).toBe('with-response')
        return { kind: 'route', payload: { bytes: 512 } }
      }
      if (command === 'connection.events.unsubscribe' || command === 'connection.disconnect') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      throw new Error(`unexpected route ${command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const effectiveMtu = manager.capabilities.get('connection:effective-mtu')
    expect(effectiveMtu).toMatchObject({ state: 'unsupported' })
    // Finding 217 follow-up: the renderer routes the control, so no
    // renderer note is appended — the native reason stands alone.
    expect(effectiveMtu.limitations.map(limitation => limitation.code)).toEqual([
      'effective-mtu-boundary-unavailable'
    ])

    const connection = await manager.connect('polar-h10')
    await expect(connection.controls.maximumWriteLength('with-response')).resolves.toMatchObject({
      state: 'measured',
      mode: 'with-response',
      maximumWriteLength: 512
    })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'connection.maximum-write-length')).toBe(
      true
    )
    await expect(connection.controls.effectiveMtu()).rejects.toMatchObject({
      code: 'capability.unsupported',
      platform: expect.objectContaining({ code: 'effective-mtu-boundary-unavailable' })
    })
    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'released' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('finding 217 follow-up: effective MTU is measured through IPC when the native snapshot reports it limited', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap')
        return {
          kind: 'bootstrap',
          bootstrap: bootstrap([
            'connection:effective-mtu',
            'connection.rssi-and-att-mtu-capability-contract',
            'limited',
            'corebluetooth-derived-effective-mtu'
          ])
        }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command } = request.envelope
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-1',
            connectionId: 'connection-id-1',
            ownerLeaseId: 'tauri-lease-1',
            peerId: 'polar-h10',
            connectionGeneration: 'generation-1'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-events-ipc-1',
            connectionId: 'connection-id-1',
            connectionGeneration: 'generation-1',
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.effective-mtu') return { kind: 'route', payload: { mtu: 515 } }
      if (command === 'connection.events.unsubscribe' || command === 'connection.disconnect') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      throw new Error(`unexpected route ${command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const effectiveMtu = manager.capabilities.get('connection:effective-mtu')
    expect(effectiveMtu).toMatchObject({ state: 'limited' })

    const connection = await manager.connect('polar-h10')
    await expect(connection.controls.effectiveMtu()).resolves.toMatchObject({
      state: 'measured',
      attMtu: 515,
      payloadBytes: 512,
      platformPduBytes: null
    })
    expect(invoke.mock.calls.some(([, args]) => args.request.envelope?.command === 'connection.effective-mtu')).toBe(
      true
    )
    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'released' })
    await expect(manager.destroy()).resolves.toMatchObject({ state: 'released' })
  })

  test('does not discover GATT when lifecycle admission fails', async () => {
    const commands = []
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command } = request.envelope
      commands.push(command)
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-lifecycle-admission-failure',
            connectionId: 'connection-id-admission-failure',
            ownerLeaseId: 'tauri-lease-1',
            peerId: 'peer-admission-failure',
            connectionGeneration: 'generation-admission-failure'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        throw new Error('lifecycle admission rejected')
      }
      if (command === 'gatt.discover') {
        throw new Error('discovery must not be routed after lifecycle admission failure')
      }
      throw new Error(`unexpected route ${command}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    const connection = await manager.connect('peer-admission-failure')
    await expect(connection.discover()).rejects.toBeDefined()
    expect(commands).toContain('connection.events.subscribe')
    expect(commands).not.toContain('gatt.discover')
    await manager.destroy()
  })

  test('runs scan, connect, GATT, notifications, and deterministic cleanup through one manager surface', async () => {
    const commands = []
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }

      const { command, payload } = request.envelope
      commands.push(command)
      if (command === 'connection.events.ready') {
        streamValue('connection-events-ipc-1', {
          kind: 'connection-lifecycle',
          schemaVersion: 2,
          attachment: bootstrap().attachment,
          attachmentId: 'tauri-attachment-1',
          peerId: 'polar-h10',
          connectionId: 'connection-id-1',
          connectionGeneration: 'generation-1',
          ownerLeaseId: 'tauri-lease-1',
          sequence: 1,
          backendIngressOrdinal: null,
          previous: 'connecting',
          current: 'connected',
          cause: 'connected'
        })
        return { kind: 'route', payload: { state: 'ready' } }
      }
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
        'connection.events.subscribe': {
          handle: 'connection-events-ipc-1',
          connectionId: 'connection-id-1',
          connectionGeneration: 'generation-1',
          eventSchemaVersion: 2
        },
        'connection.events.ready': { state: 'ready' },
        'connection.events.unsubscribe': { state: 'released', failures: [] },
        'gatt.discover': {
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
              properties: ['notify', 'read', 'write']
            }
          ],
          descriptors: [
            { handle: 'descriptor-1', characteristicHandle: 'characteristic-1', uuid: '2901', occurrence: '0' }
          ]
        },
        'gatt.read': { value: { $__unifiedBleBytesV2: [1, 2, 3] }, provenance: 'read-or-notification' },
        'gatt.write': {
          terminal: { correlation: 'write-operation-1', outcome: 'succeeded', cause: null },
          mode: 'with-response',
          commitState: 'confirmed',
          bytesSubmitted: 2
        },
        'gatt.descriptor.write': {
          terminal: { correlation: 'descriptor-write-operation-1', outcome: 'succeeded', cause: null },
          mode: 'with-response',
          commitState: 'confirmed',
          bytesSubmitted: 1
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
    const scan = await manager.scan({})
    const observation = scan.observations[Symbol.asyncIterator]().next()
    streamValue('scan-1', advertisement('polar-h10', 'Polar H10', -47))
    await expect(observation).resolves.toMatchObject({
      value: { kind: 'value', value: { peer: { id: 'polar-h10' }, localName: 'Polar H10', rssi: -47 } }
    })
    await expect(scan.stop()).resolves.toMatchObject({ state: 'released' })

    const connection = await manager.connect('polar-h10')
    expect(connection.connectionId).toBe('connection-id-1')
    expect(connection.events).toBeUndefined()
    const lifecycle = connection.lifecycleEvents[Symbol.asyncIterator]().next()
    const secondLifecycle = connection.lifecycleEvents[Symbol.asyncIterator]().next()
    await expect(lifecycle).resolves.toMatchObject({
      value: {
        kind: 'connection-lifecycle',
        current: 'connected',
        connectionGeneration: 'generation-1'
      }
    })
    await expect(secondLifecycle).resolves.toMatchObject({
      value: {
        kind: 'connection-lifecycle',
        current: 'connected',
        connectionGeneration: 'generation-1'
      }
    })
    const database = await connection.discover()
    expect(database.generation).toBe('database-generation-1')
    const characteristic = database.service('180d').characteristic('2a37')
    await expect(characteristic.read()).resolves.toEqual(new Uint8Array([1, 2, 3]))
    await expect(characteristic.readReceipt()).resolves.toEqual({
      value: new Uint8Array([1, 2, 3]),
      provenance: 'read-or-notification'
    })
    await expect(characteristic.write(new Uint8Array([4, 5]), { response: 'required' })).resolves.toMatchObject({
      terminal: { correlation: 'write-operation-1', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
    })
    expect(database.snapshot().characteristics).toHaveLength(1)
    expect(database.snapshot().descriptors[0].properties).toMatchObject({
      read: false,
      write: false,
      availability: { read: 'unknown', write: 'unknown' }
    })
    const gattWriteRequest = invoke.mock.calls.find(([, args]) => args.request.envelope?.command === 'gatt.write')
    expect(gattWriteRequest?.[1].request.envelope.payload).toMatchObject({
      databaseId: 'database-id-1',
      databaseGeneration: 'database-generation-1',
      connectionId: 'connection-id-1',
      ownerLeaseId: 'tauri-lease-1',
      connectionGeneration: 'generation-1',
      peerId: 'polar-h10'
    })
    const descriptor = characteristic.descriptor('2901')
    await expect(descriptor.write(new Uint8Array([1]), { response: 'required' })).resolves.toMatchObject({
      terminal: { correlation: 'descriptor-write-operation-1', outcome: 'succeeded', cause: null },
      commitState: 'confirmed'
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
      'connection.events.subscribe',
      'connection.events.ready',
      'gatt.discover',
      'gatt.read',
      'gatt.read',
      'gatt.write',
      'gatt.descriptor.write',
      'gatt.subscribe',
      'gatt.unsubscribe',
      'connection.events.unsubscribe',
      'connection.disconnect'
    ])
  })

  // PR210-37: a native terminal error states its commit state; the IPC
  // manager carries a well-formed one and refuses an unknown word.
  test.each([
    ['uncertain', 'connection-lost'],
    [null, 'connection-lost'],
    ['committed', 'source-failed']
  ])('a notification terminal with commit %p ends the stream %s', async (commit, expectedReason) => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command } = request.envelope
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      const responses = {
        'connection.connect': {
          handle: 'connection-1',
          connectionId: 'connection-id-1',
          ownerLeaseId: 'tauri-lease-1',
          peerId: 'polar-h10',
          connectionGeneration: 'generation-1'
        },
        'connection.events.subscribe': {
          handle: 'connection-events-ipc-1',
          connectionId: 'connection-id-1',
          connectionGeneration: 'generation-1',
          eventSchemaVersion: 2
        },
        'connection.events.unsubscribe': { state: 'released', failures: [] },
        'gatt.discover': {
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
              properties: ['notify']
            }
          ],
          descriptors: []
        },
        'gatt.subscribe': { handle: 'subscription-1', delivery: 'unknown' },
        'gatt.unsubscribe': { state: 'released', failures: [] },
        'connection.disconnect': { state: 'released', failures: [] }
      }
      return { kind: 'route', payload: responses[command] }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const connection = await manager.connect('polar-h10')
    const database = await connection.discover()
    const subscription = await database.service('180d').characteristic('2a37').subscribe()
    const ended = subscription.values[Symbol.asyncIterator]().next()
    FakeChannel.current.emit({
      rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' },
      eventId: 'subscription-1-terminal',
      streamId: 'subscription-1',
      item: {
        kind: 'terminal',
        reason: 'connection-lost',
        error: {
          code: 'operation.disconnected',
          domain: 'connection',
          operation: 'gatt.notifications',
          platform: null,
          retryability: 'never',
          commit
        }
      }
    })
    await expect(ended).resolves.toMatchObject({ value: { kind: 'terminal', reason: expectedReason } })
    await manager.destroy()
  })

  test.each([
    ['a malformed platform UUID', 'protocol.malformed', 'gatt', 'discovery.snapshot.uuid'],
    ['a database past the ATT handle space', 'capability.limited', 'gatt', 'discovery.database-bound'],
    ['a discovery without a core lease', 'argument.invalid', 'core', 'path.owner']
  ])(
    'surfaces the core refusing %s as the same typed discovery failure (finding 95)',
    async (_case, code, domain, operation) => {
      const commands = []
      const invoke = jest.fn(async (_command, args) => {
        const request = args.request
        if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
        if (request.kind === 'event.ack') return { kind: 'event.ack' }
        if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
        const { command } = request.envelope
        commands.push(command)
        if (command === 'connection.connect') {
          return {
            kind: 'route',
            payload: {
              handle: 'connection-discovery-failure',
              connectionId: 'connection-id-discovery-failure',
              ownerLeaseId: 'tauri-lease-1',
              peerId: 'polar-h10',
              connectionGeneration: 'generation-discovery-failure'
            }
          }
        }
        if (command === 'connection.events.subscribe') {
          return {
            kind: 'route',
            payload: {
              handle: 'connection-events-ipc-1',
              connectionId: 'connection-id-discovery-failure',
              connectionGeneration: 'generation-discovery-failure',
              eventSchemaVersion: 2
            }
          }
        }
        if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
        if (command === 'gatt.discover') {
          return {
            kind: 'failure',
            error: { code, domain, operation, platform: null, retryability: 'never' }
          }
        }
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      })
      const { BleError } = require('../src')
      const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
      const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
      const connection = await manager.connect('polar-h10')

      const failure = await connection.discover().then(
        () => null,
        error => error
      )
      expect(failure).toBeInstanceOf(BleError)
      expect(failure).toMatchObject({ code, domain, operation })
      expect(commands.filter(command => command === 'gatt.discover')).toHaveLength(1)
      await manager.destroy()
    }
  )

  // Finding 116: the public error carries the OS's own identity exactly as
  // the Node desktop path reports it (here WinRT with HRESULT and ATT status).
  test('rehydrates the native platform identity onto the public BleError', async () => {
    const platform = {
      domain: 'winrt',
      code: 'gatt-protocol-error',
      safeMessage: 'The attribute requires authentication.',
      metadata: { hresult: -2140864509, gattStatus: 5 }
    }
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
          operation: 'connection.connect',
          platform,
          retryability: 'never',
          commit: null
        }
      }
    })
    const { BleError } = require('../src')
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    const failure = await manager.connect('polar-h10').then(
      () => null,
      error => error
    )
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.platform).toEqual(platform)
    await manager.destroy()
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

  test('rejects contradictory write receipt outcomes before exposing them', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command } = request.envelope
      if (command === 'connection.connect') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-receipt',
            connectionId: 'connection-id-receipt',
            ownerLeaseId: 'tauri-lease-1',
            peerId: 'polar-h10',
            connectionGeneration: 'generation-receipt'
          }
        }
      }
      if (command === 'connection.events.subscribe') {
        return {
          kind: 'route',
          payload: {
            handle: 'connection-events-ipc-1',
            connectionId: 'connection-id-receipt',
            connectionGeneration: 'generation-receipt',
            eventSchemaVersion: 2
          }
        }
      }
      if (command === 'connection.events.ready') return { kind: 'route', payload: { state: 'ready' } }
      if (command === 'connection.events.unsubscribe') {
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      if (command === 'gatt.discover') {
        return {
          kind: 'route',
          payload: {
            schemaVersion: 2,
            handle: 'database-receipt',
            databaseId: 'database-id-receipt',
            databaseGeneration: 'database-generation-receipt',
            services: [{ uuid: '180d', occurrence: '0', primary: true, includedServices: [] }],
            characteristics: [
              {
                handle: 'characteristic-receipt',
                serviceUuid: '180d',
                serviceOccurrence: '0',
                characteristicUuid: '2a37',
                characteristicOccurrence: '0',
                properties: ['write']
              }
            ],
            descriptors: []
          }
        }
      }
      if (command === 'gatt.write') {
        return {
          kind: 'route',
          payload: {
            terminal: {
              correlation: 'write-operation-contradictory',
              outcome: 'succeeded',
              cause: 'gatt.write-failed'
            },
            mode: 'with-response',
            commitState: 'confirmed',
            bytesSubmitted: 1
          }
        }
      }
      return { kind: 'route', payload: { state: 'released', failures: [] } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const database = await (await manager.connect('polar-h10')).discover()
    const snapshot = await database.snapshot()

    const characteristic = database.service('180d').characteristic('2a37')
    await expect(characteristic.write(new Uint8Array([1]), { response: 'required' })).rejects.toMatchObject({
      code: 'protocol.malformed'
    })
    await manager.destroy()
  })

  test('rejects connect identities that do not match the request or renderer lease', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return {
        kind: 'route',
        payload: {
          handle: 'connection-identity',
          connectionId: 'connection-id-identity',
          ownerLeaseId: 'foreign-lease',
          peerId: 'different-peer',
          connectionGeneration: 'generation-identity'
        }
      }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    const connectError = await manager.connect('polar-h10').then(
      () => null,
      error => error
    )
    expect(connectError).toBeInstanceOf(AggregateError)
    expect(
      connectError.errors.some(
        error => error?.normalized?.code === 'protocol.violation' || error?.code === 'protocol.violation'
      )
    ).toBe(true)
    await manager.destroy().catch(() => undefined)
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

    const releaseError = await connection.release().then(
      () => null,
      error => error
    )
    expect(releaseError?.code ?? releaseError?.normalized?.code).toBe('connection.lost')
    await manager.destroy().catch(() => undefined)
  })

  test('propagates AbortSignal cancellation through the shared IPC client', async () => {
    let resolveConnect
    // The cancellation is dispatched fire-and-forget by the abort listener, so
    // nothing in the public API exposes it to await. The mock is the one place
    // the test can see it, so it settles a promise instead of being polled.
    let cancelRouted
    const cancelSeen = new Promise(resolve => {
      cancelRouted = resolve
    })
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.envelope.command === 'operation.cancel') {
        cancelRouted(request)
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
    await awaitSignal(cancelSeen, 'operation.cancel routed to native')
    await manager.destroy()
  })

  test('turns a manager timeout into native operation cancellation', async () => {
    let resolveConnect
    // The cancellation is dispatched fire-and-forget by the abort listener, so
    // nothing in the public API exposes it to await. The mock is the one place
    // the test can see it, so it settles a promise instead of being polled.
    let cancelRouted
    const cancelSeen = new Promise(resolve => {
      cancelRouted = resolve
    })
    // A cancellation is routed to native only for an operation that reached the
    // `dispatched` phase: the coordinator's `queued` and `admitting` branches
    // settle the caller and return without one, correctly, because there is
    // nothing on the far side to cancel. So this test must PROVE the connect
    // was dispatched before the deadline expires, rather than assume a 1ms
    // timer loses a race it is not guaranteed to lose - on a loaded two-core
    // runner it can win, leaving the test waiting for a cancellation the
    // product was right never to send.
    let connectDispatched
    const connectSeen = new Promise(resolve => {
      connectDispatched = resolve
    })
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.envelope.command === 'operation.cancel') {
        cancelRouted(request)
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
      connectDispatched()
      return new Promise(resolve => {
        resolveConnect = resolve
      })
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    // Fake timers only after bootstrap, so the deadline is the one timer under
    // the test's control. Promise progression is unaffected by them, which is
    // what lets the dispatch be awaited before the clock is advanced.
    jest.useFakeTimers()
    try {
      const connecting = manager.connect('polar-h10', { timeoutMs: 1 })
      connecting.catch(() => undefined)
      // Awaited directly rather than through awaitSignal: fake timers replace
      // the very setTimeout that helper's failure path depends on, so its
      // budget could never fire here. Jest's own timeout still bounds a hang -
      // it just reports it less specifically, which is the honest trade for
      // owning the clock.
      await connectSeen

      // Only now can the deadline expire, and only against a dispatched
      // operation - so a routed cancellation is a guarantee, not a race.
      // Finding 161: a dispatched connect whose deadline expires before any
      // link came up is the peer not answering - `connection.failed`
      // (caller-decides) on every backend, never `operation.timed-out`.
      jest.advanceTimersByTime(2)
      await expect(connecting).rejects.toMatchObject({
        code: 'connection.failed',
        retryability: 'caller-decides',
        platform: { domain: 'ipc', code: 'deadline-expired' }
      })
      await expect(connecting.catch(error => error)).resolves.toMatchObject({
        platform: { metadata: { deadlineMs: 1 } }
      })
      await cancelSeen
    } finally {
      jest.useRealTimers()
    }
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
    expect(first.value).toMatchObject({ kind: 'value', value: { peer: { id: 'early-peer' }, localName: 'Early' } })
    await scan.stop()
    await manager.destroy()
  })

  test('stops an active public scan when its AbortSignal is cancelled after start', async () => {
    const scanStops = []
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.envelope.command === 'scan.start') return { kind: 'route', payload: { handle: 'scan-abort' } }
      if (request.envelope.command === 'scan.stop') {
        scanStops.push(request.envelope.payload.scanHandle)
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      }
      return { kind: 'route', payload: { accepted: true } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const controller = new AbortController()
    const scan = await manager.scan({ signal: controller.signal })
    controller.abort()
    for (let turn = 0; turn < 4; turn += 1) await Promise.resolve()
    expect(scanStops).toEqual(['scan-abort'])
    await manager.destroy()
  })

  test('rejects bootstrap records with contradictory identity or adapter state', async () => {
    const invalidBootstrap = bootstrap()
    invalidBootstrap.attachmentId = 'mismatched-attachment'
    const invoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: invalidBootstrap }))
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')

    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toMatchObject({
      code: 'protocol.malformed'
    })

    const invalidStateBootstrap = bootstrap()
    invalidStateBootstrap.attachment.adapter.state.power = 'not-a-power-state'
    const invalidStateInvoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: invalidStateBootstrap }))

    await expect(
      createTauriBleManagerWithEnvironment({ invoke: invalidStateInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ code: 'protocol.malformed' })

    const invalidCapabilityBootstrap = bootstrap()
    invalidCapabilityBootstrap.capabilities.descriptors[0].evidence.sourceDigest = ''
    const invalidCapabilityInvoke = jest.fn(async () => ({
      kind: 'bootstrap',
      bootstrap: invalidCapabilityBootstrap
    }))
    await expect(
      createTauriBleManagerWithEnvironment({ invoke: invalidCapabilityInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ code: 'protocol.malformed' })

    const mismatchedGenerationBootstrap = bootstrap()
    mismatchedGenerationBootstrap.attachment.adapter.state.backendGeneration = 'other-generation'
    const mismatchedGenerationInvoke = jest.fn(async () => ({
      kind: 'bootstrap',
      bootstrap: mismatchedGenerationBootstrap
    }))
    await expect(
      createTauriBleManagerWithEnvironment({ invoke: mismatchedGenerationInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ code: 'protocol.malformed' })

    const outOfRangeVersionBootstrap = bootstrap()
    outOfRangeVersionBootstrap.versions.ipcProtocol.selected = { axis: 'ipc-protocol', value: 5 }
    const outOfRangeVersionInvoke = jest.fn(async () => ({ kind: 'bootstrap', bootstrap: outOfRangeVersionBootstrap }))
    await expect(
      createTauriBleManagerWithEnvironment({ invoke: outOfRangeVersionInvoke, Channel: FakeChannel })
    ).rejects.toMatchObject({ code: 'protocol.malformed' })
  })

  test('rejects malformed public filters instead of silently scanning without them', async () => {
    const invoke = jest.fn(async (_command, args) => {
      if (args.request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (args.request.kind === 'event.ack') return { kind: 'event.ack' }
      if (args.request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (args.request.envelope?.command === 'scan.stop')
        return { kind: 'route', payload: { state: 'released', failures: [] } }
      return { kind: 'route', payload: { handle: 'scan-should-not-start' } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    const latest = await manager.scan({ delivery: 'latest' })
    const scanStart = invoke.mock.calls.find(([, args]) => args.request.envelope?.command === 'scan.start')
    expect(scanStart?.[1].request.envelope.payload).toMatchObject({
      streamItemCapacity: 1,
      streamOverflowPolicy: 'drop-oldest'
    })
    await latest.stop()
    const scanStartCount = invoke.mock.calls.filter(
      ([, args]) => args.request.envelope?.command === 'scan.start'
    ).length

    for (const filter of [{ serviceUuids: 'not-an-array' }, [], new Date()]) {
      await expect(manager.scan({ filter })).rejects.toMatchObject({ code: 'argument.invalid' })
    }
    expect(invoke.mock.calls.filter(([, args]) => args.request.envelope?.command === 'scan.start')).toHaveLength(
      scanStartCount
    )
    await manager.destroy()
  })

  test('releases the attached IPC lease when host option admission fails', async () => {
    const invoke = jest.fn(async (_command, args) => {
      if (args.request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (args.request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: { accepted: true } }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')

    await expect(
      createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel }, { adapterId: 'wrong-adapter' })
    ).rejects.toMatchObject({ code: 'adapter.unavailable' })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])

    invoke.mockClear()
    await expect(
      createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel }, { restoration: { restorationId: 'ble' } })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])

    invoke.mockClear()
    await expect(
      createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel }, { instanceId: 'named' })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])

    invoke.mockClear()
    await expect(
      createTauriBleManagerWithEnvironment(
        { invoke, Channel: FakeChannel },
        { diagnostics: { traceMaximumRecords: 8 } }
      )
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])
  })

  test('use after destroy rejects with a public lifecycle BleError without Tauri wording', async () => {
    const invoke = jest.fn(async (_command, args) => {
      if (args.request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (args.request.kind === 'event.ack') return { kind: 'event.ack' }
      if (args.request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'route', payload: {} }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
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
    await expect(manager.adapter.state()).rejects.toEqual(expect.not.objectContaining({ name: 'TypeError' }))
    await manager.adapter.state().catch(error => {
      expect(String(error)).not.toMatch(/Tauri/i)
      expect(error).not.toBeInstanceOf(TypeError)
    })
  })
})

describe('Tauri shared-core admission (F01)', () => {
  const { CONTRACT_REVISION } = require('../contracts/src/version')
  const { TAURI_PLUGIN_COMPATIBILITY } = require('../src/tauri')

  function invokeWithBootstrap(bootstrapValue) {
    return jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrapValue }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected route ${request.kind}`)
    })
  }

  test('compatibility pins the frozen contract revision', () => {
    expect(TAURI_PLUGIN_COMPATIBILITY.contractRevision).toBe(CONTRACT_REVISION)
  })

  test('admits the candidate plugin identity', async () => {
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({
      invoke: invokeWithBootstrap(bootstrap()),
      Channel: FakeChannel
    })
    await manager.destroy()
  })

  test('rejects a bootstrap without core identity and releases it', async () => {
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const { core, ...legacy } = bootstrap()
    expect(core).toBeDefined()
    const invoke = invokeWithBootstrap(legacy)
    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toThrow(
      /contract revision/i
    )
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])
  })

  test('rejects a foreign contract revision and releases it', async () => {
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const foreign = {
      ...bootstrap(),
      core: { contractRevision: 'C-UBM.9.9.9-DRAFT', implementationVersion: '5.0.0-rc.3' }
    }
    const invoke = invokeWithBootstrap(foreign)
    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toThrow(
      /contract revision/i
    )
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap', 'release'])
  })

  test('rejects a malformed core identity at the transport before attach', async () => {
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const malformed = { ...bootstrap(), core: { contractRevision: 42 } }
    const invoke = invokeWithBootstrap(malformed)
    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toThrow()
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap'])
  })
})

// PR210-06 / PR210-22: the Tauri deadline path end to end through the shared
// IPC manager. The webview sends a relative budget, the deadline still routes
// an exact-operation cancel, and the native answer about retryability survives
// the manager's aborted -> timed-out remap.
describe('Tauri deadline and native retryability through the IPC manager', () => {
  const nativePlatform = {
    domain: 'btleplug',
    code: 'write-dispatched',
    safeMessage: 'The write reached the radio before the deadline.',
    metadata: {}
  }

  async function writeHarness(nativeAnswer) {
    const routed = []
    let resolveWrite
    let writeDispatched
    const writeSeen = new Promise(resolve => {
      writeDispatched = resolve
    })
    let cancelRouted
    const cancelSeen = new Promise(resolve => {
      cancelRouted = resolve
    })
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      routed.push(request.envelope)
      if (request.envelope.command === 'operation.cancel') {
        cancelRouted(request.envelope)
        resolveWrite({ kind: 'failure', error: nativeAnswer })
        return { kind: 'route', payload: { state: 'cancellation-requested' } }
      }
      writeDispatched(request.envelope)
      return new Promise(resolve => {
        resolveWrite = resolve
      })
    })
    const { IpcBleManager } = require('../src/ipc/manager')
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const ipc = await IpcBleManager.create(new TauriBleIpcTransport({ invoke, Channel: FakeChannel }))
    return { ipc, routed, writeSeen, cancelSeen }
  }

  async function expireWrite(harness, budgetMs) {
    jest.useFakeTimers()
    try {
      const writing = harness.ipc.route(
        'gatt.write',
        { handle: 'characteristic-1', deadline: globalThis.performance.now() + budgetMs },
        new Uint8Array([1, 2, 3])
      )
      writing.catch(() => undefined)
      const writeEnvelope = await harness.writeSeen
      jest.advanceTimersByTime(budgetMs + 1)
      const failure = await writing.then(
        () => null,
        error => error
      )
      const cancelEnvelope = await harness.cancelSeen
      return { writeEnvelope, cancelEnvelope, failure }
    } finally {
      jest.useRealTimers()
    }
  }

  test('a deadline expiry cancels the exact dispatched operation that carried a relative budget', async () => {
    const harness = await writeHarness({
      code: 'operation.aborted',
      domain: 'gatt',
      operation: 'tauri.gatt.write',
      platform: nativePlatform,
      retryability: 'never'
    })

    const { writeEnvelope, cancelEnvelope } = await expireWrite(harness, 50)

    expect(writeEnvelope.payload).not.toHaveProperty('deadline')
    expect(Number.isSafeInteger(writeEnvelope.payload.budgetMs)).toBe(true)
    expect(writeEnvelope.payload.budgetMs).toBeGreaterThanOrEqual(0)
    expect(writeEnvelope.payload.budgetMs).toBeLessThanOrEqual(50)
    expect(cancelEnvelope.payload).toEqual({ targetCorrelation: String(writeEnvelope.correlation) })
    expect(harness.routed.filter(envelope => envelope.command === 'gatt.write')).toHaveLength(1)
    await harness.ipc.destroy()
  })

  test('a dispatched write that expires keeps the native never and platform detail', async () => {
    const harness = await writeHarness({
      code: 'operation.aborted',
      domain: 'gatt',
      operation: 'tauri.gatt.write',
      platform: nativePlatform,
      retryability: 'never'
    })

    const { failure } = await expireWrite(harness, 50)

    expect(failure?.normalized).toEqual({
      code: 'operation.timed-out',
      domain: 'gatt',
      operation: 'tauri.gatt.write',
      platform: nativePlatform,
      retryability: 'never'
    })
    expect(harness.routed.filter(envelope => envelope.command === 'gatt.write')).toHaveLength(1)
    await harness.ipc.destroy()
  })

  test('an expiry the core answers before dispatch keeps caller-decides', async () => {
    const harness = await writeHarness({
      code: 'operation.aborted',
      domain: 'gatt',
      operation: 'tauri.gatt.write',
      platform: null,
      retryability: 'caller-decides'
    })

    const { failure } = await expireWrite(harness, 50)

    expect(failure?.normalized).toMatchObject({ code: 'operation.timed-out', retryability: 'caller-decides' })
    await harness.ipc.destroy()
  })

  test('a native failure that is not an abort is rethrown unchanged after the deadline', async () => {
    const nativeAnswer = {
      code: 'gatt.write-failed',
      domain: 'gatt',
      operation: 'tauri.gatt.write',
      platform: nativePlatform,
      retryability: 'never'
    }
    const harness = await writeHarness(nativeAnswer)

    const { failure } = await expireWrite(harness, 50)

    expect(failure?.normalized).toEqual(nativeAnswer)
    await harness.ipc.destroy()
  })
})

// PR210-73: the webview/plugin wire changed (relative `budgetMs`, `commit` on
// errors, subscribe `delivery`, lifecycle events), so the IPC protocol is 3.
// A mixed pair fails at bootstrap as protocol.incompatible, never later as
// protocol.malformed on its first operation.
describe('Tauri IPC protocol version 3', () => {
  function versionAxes(ipcProtocol) {
    const bootstrapValue = bootstrap()
    const selected = { axis: 'ipc-protocol', value: ipcProtocol }
    const range = { axis: 'ipc-protocol', minimum: selected, maximum: selected }
    bootstrapValue.versions.ipcProtocol = { axis: 'ipc-protocol', selected, localRange: range, remoteRange: range }
    return bootstrapValue
  }

  test('the webview offers exactly IPC protocol 4', async () => {
    const { TAURI_PLUGIN_COMPATIBILITY } = require('../src/tauri/compatibility')
    const { IPC_PROTOCOL_VERSION } = require('../src/ipc/protocol')
    expect(IPC_PROTOCOL_VERSION).toBe(4)
    expect(TAURI_PLUGIN_COMPATIBILITY.ipcProtocol).toBe(4)

    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: bootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      return { kind: 'event.ack' }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const offer = invoke.mock.calls[0][1].request.offer
    expect(offer.ipcProtocol).toEqual({
      axis: 'ipc-protocol',
      minimum: { axis: 'ipc-protocol', value: 4 },
      maximum: { axis: 'ipc-protocol', value: 4 }
    })
    await manager.destroy()
  })

  test('an old plugin that cannot serve protocol 4 fails as protocol.incompatible before any operation', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind !== 'bootstrap') throw new Error(`unexpected ${request.kind}`)
      // A protocol-3 plugin (5.0 pre-rebind) negotiates only 3 against the offer.
      const range = request.offer.ipcProtocol
      if (range.minimum.value > 3 || range.maximum.value < 3) {
        return {
          kind: 'failure',
          error: {
            code: 'protocol.incompatible',
            domain: 'ipc',
            operation: 'tauri.bootstrap-version-ipc-protocol',
            platform: null,
            retryability: 'never'
          }
        }
      }
      return { kind: 'bootstrap', bootstrap: versionAxes(3) }
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')

    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toMatchObject({
      code: 'protocol.incompatible'
    })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).toEqual(['bootstrap'])
  })

  test('a plugin that selects protocol 3 is refused as protocol.incompatible and released', async () => {
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: versionAxes(3) }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      throw new Error(`unexpected ${request.kind}`)
    })
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')

    await expect(createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })).rejects.toMatchObject({
      code: 'protocol.incompatible'
    })
    expect(invoke.mock.calls.map(([, args]) => args.request.kind)).not.toContain('route')
  })
})
