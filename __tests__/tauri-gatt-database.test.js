// __tests__/tauri-gatt-database.test.js
//
// Finding 182: the Polar H10 database over the Tauri/Electron IPC path.
// The Tauri dispatcher fanned one characteristic record out per descriptor
// row, so every H10 characteristic with a descriptor (heart-rate
// measurement, battery level, device-info strings, PMD ECG) reached the
// public snapshot as duplicated paths and failed with
// `protocol.violation: public-gatt.duplicate-characteristic-path` — while
// the desktop N-API path (groupCorePaths) renders one record per
// characteristic. Electron's renderer consumes the same shared IPC codec
// (`IpcGattDatabase`), so both hosts decode here.

'use strict'

const { createPublicGattDatabase } = require('../src/public/gatt')
const { IpcGattDatabase } = require('../src/ipc/manager')
const { BleError } = require('../src/public/errors')

const CCCD = '2902'

const H10_SERVICES = ['1800', '1801', '180d', '180a', '180f', '6217ff4b-fb31-1140-ad5a-a45545d7ecf3', 'fb005c80-02e7-f387-1cad-8acd2d8df0c8', 'feee']

// [serviceUuid, characteristicUuid, properties, descriptorUuids]
const H10_CHARACTERISTICS = [
  ['1800', '2a00', ['read'], []],
  ['1800', '2a01', ['read'], []],
  ['1801', '2a05', ['indicate'], []],
  ['180d', '2a37', ['notify'], [CCCD]],
  ['180d', '2a38', ['read'], []],
  ['180d', '2a39', ['write'], []],
  ['180a', '2a29', ['read'], ['2901']],
  ['180a', '2a24', ['read'], []],
  ['180f', '2a19', ['read', 'notify'], [CCCD]],
  ['6217ff4b-fb31-1140-ad5a-a45545d7ecf3', '6217ff4c-fb31-1140-ad5a-a45545d7ecf3', ['read', 'write'], []],
  ['fb005c80-02e7-f387-1cad-8acd2d8df0c8', 'fb005c81-02e7-f387-1cad-8acd2d8df0c8', ['notify'], [CCCD]],
  ['feee', 'feef', ['read'], []]
]

function databasePath() {
  return {
    attachment: {},
    attachmentId: 'attachment-h10',
    peerId: 'peer-h10',
    connectionId: 'connection-h10',
    ownerLeaseId: 'lease-h10',
    connectionGeneration: 'generation-h10',
    databaseId: 'database-h10',
    databaseGeneration: 'generation-h10'
  }
}

function portableProperties(names) {
  return {
    read: names.includes('read'),
    writeWithResponse: names.includes('write'),
    writeWithoutResponse: names.includes('write-without-response'),
    notify: names.includes('notify'),
    indicate: names.includes('indicate')
  }
}

// The desktop N-API shape: one portable record per GATT attribute, with the
// core's per-UUID occurrence numerals.
function h10PortableSnapshot() {
  const path = databasePath()
  const services = H10_SERVICES.map(serviceUuid => ({
    path: { ...path, serviceUuid, serviceOccurrence: '0' },
    primary: true,
    includedServices: []
  }))
  const characteristics = H10_CHARACTERISTICS.map(([serviceUuid, characteristicUuid, properties]) => ({
    path: {
      ...path,
      serviceUuid,
      serviceOccurrence: '0',
      characteristicUuid,
      characteristicOccurrence: '0',
      validity: 'current'
    },
    properties: portableProperties(properties)
  }))
  const descriptors = H10_CHARACTERISTICS.flatMap(([serviceUuid, characteristicUuid, , descriptorUuids]) =>
    descriptorUuids.map(descriptorUuid => ({
      path: {
        ...path,
        serviceUuid,
        serviceOccurrence: '0',
        characteristicUuid,
        characteristicOccurrence: '0',
        descriptorUuid,
        descriptorOccurrence: '0',
        validity: 'current'
      }
    }))
  )
  return { path, services, characteristics, descriptors }
}

function sourceFor(snapshot) {
  return { snapshot: async () => snapshot }
}

// The Tauri `gatt.discover` wire shape. `duplicated: true` reproduces the
// pre-fix dispatcher encoding: one characteristic record per descriptor row
// (the physical failure); `false` is the fixed encoding, one record per
// characteristic — identical occurrence numerals to the desktop core.
function h10WirePayload(duplicated, tag = 'h10') {
  let handle = 0
  const next = prefix => `${prefix}-${tag}-${(handle += 1)}`
  const characteristics = []
  const descriptors = []
  for (const [serviceUuid, characteristicUuid, properties, descriptorUuids] of H10_CHARACTERISTICS) {
    const rows = duplicated ? 1 + descriptorUuids.length : 1
    for (let row = 0; row < rows; row += 1) {
      const characteristicHandle = next('characteristic')
      characteristics.push({
        handle: characteristicHandle,
        serviceUuid,
        serviceOccurrence: '0',
        characteristicUuid,
        characteristicOccurrence: '0',
        properties
      })
      if (row === 0) {
        for (const uuid of descriptorUuids) {
          descriptors.push({ handle: next('descriptor'), characteristicHandle, uuid, occurrence: '0' })
        }
      }
    }
  }
  return {
    schemaVersion: 2,
    handle: `database-${tag}`,
    databaseId: `database-id-${tag}`,
    databaseGeneration: `database-generation-${tag}`,
    services: H10_SERVICES.map(uuid => ({ uuid, occurrence: '0', primary: true, includedServices: [] })),
    characteristics,
    descriptors
  }
}

function stubIpcLink() {
  const attachment = {
    attachmentId: 'attachment-h10',
    backendInstanceId: 'backend-h10',
    backendGeneration: 'backend-generation-h10',
    adapter: { adapterId: 'adapter-h10', adapterGeneration: 'adapter-generation-h10' }
  }
  const manager = { bootstrap: { attachment } }
  const connection = {
    peerId: 'peer-h10',
    connectionId: 'connection-h10',
    ownerLeaseId: 'lease-h10',
    connectionGeneration: 'generation-h10',
    registerDatabase() {}
  }
  return { manager, connection }
}

async function decodeWirePayload(payload) {
  const { manager, connection } = stubIpcLink()
  const database = IpcGattDatabase.fromPayload(manager, connection, payload)
  return database.snapshot()
}

function graphOf(database) {
  return {
    services: database.services.map(service => `${service.uuid}#${service.occurrence}`),
    characteristics: database.snapshot().characteristics.map(characteristic => characteristic.uuid),
    descriptors: database.snapshot().descriptors.map(descriptor => descriptor.uuid)
  }
}

describe('Tauri/Electron H10 GATT database (finding 182)', () => {
  test('the H10 database validates on the public snapshot with desktop occurrence numerals', async () => {
    const database = await createPublicGattDatabase(sourceFor(h10PortableSnapshot()))
    expect(database.services).toHaveLength(8)
    const heartRate = database.service('180d').characteristic('2a37')
    expect(heartRate.occurrence).toBe(0)
    expect(heartRate.properties.notify).toBe(true)
    expect(heartRate.descriptor(CCCD).occurrence).toBe(0)
    expect(database.service('180f').characteristic('2a19').descriptors).toHaveLength(1)
    expect(database.service('180a').characteristic('2a29').descriptor('2901').occurrence).toBe(0)
    expect(database.snapshot().characteristics).toHaveLength(12)
    expect(database.snapshot().descriptors).toHaveLength(4)
  })

  test('the fixed Tauri encoding decodes to the desktop snapshot on both IPC hosts', async () => {
    const desktop = await createPublicGattDatabase(sourceFor(h10PortableSnapshot()))
    const tauriSnapshot = await decodeWirePayload(h10WirePayload(false))
    const electronSnapshot = await decodeWirePayload(h10WirePayload(false))
    expect(electronSnapshot).toEqual(tauriSnapshot)
    const tauri = await createPublicGattDatabase(sourceFor(tauriSnapshot))
    const desktopGraph = graphOf(desktop)
    expect(graphOf(tauri)).toEqual(desktopGraph)
    expect(tauri.service('180d').characteristic('2a37').occurrence).toBe(0)
  })

  test('the duplicated Tauri encoding fails with the offending characteristic path', async () => {
    const snapshot = await decodeWirePayload(h10WirePayload(true))
    const failure = await createPublicGattDatabase(sourceFor(snapshot)).then(
      () => null,
      error => error
    )
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.code).toBe('protocol.violation')
    expect(failure.operation).toBe('public-gatt.duplicate-characteristic-path')
    expect(failure.platform).not.toBeNull()
    expect(failure.platform.metadata).toMatchObject({
      serviceUuid: expect.stringMatching(/180d/i),
      characteristicUuid: expect.stringMatching(/2a37/i),
      serviceOccurrence: '0',
      characteristicOccurrence: '0'
    })
  })

  test.each([
    ['duplicate service path', 'public-gatt.duplicate-service-path', 'serviceUuid'],
    ['orphan characteristic parent', 'public-gatt.characteristic-parent', 'characteristicUuid'],
    ['duplicate descriptor path', 'public-gatt.duplicate-descriptor-path', 'descriptorUuid']
  ])('topology sibling %s names its offending path', async (_label, operation, uuidKey) => {
    const snapshot = h10PortableSnapshot()
    if (operation === 'public-gatt.duplicate-service-path') {
      snapshot.services = [...snapshot.services, { ...snapshot.services[0] }]
    } else if (operation === 'public-gatt.characteristic-parent') {
      snapshot.services = snapshot.services.slice(1)
    } else {
      snapshot.descriptors = [...snapshot.descriptors, { ...snapshot.descriptors[0] }]
    }
    const failure = await createPublicGattDatabase(sourceFor(snapshot)).then(
      () => null,
      error => error
    )
    expect(failure).toBeInstanceOf(BleError)
    expect(failure.code).toBe('protocol.violation')
    expect(failure.operation).toBe(operation)
    expect(failure.platform).not.toBeNull()
    expect(String(failure.platform.metadata[uuidKey] ?? failure.platform.metadata.serviceUuid)).not.toBe('')
  })

  test('connect, discover, discover replaces the snapshot instead of appending', async () => {
    const { BUILT_IN_FEATURE_IDS } = require('../src/backend-contract/capabilities')

    class FakeChannel {
      constructor() {
        this.onmessage = null
        FakeChannel.current = this
      }

      emit(message) {
        this.onmessage?.(message)
      }
    }

    function negotiated(axis, value = 1) {
      const selected = { axis, value }
      const range = { axis, minimum: selected, maximum: selected }
      return { axis, selected, localRange: range, remoteRange: range }
    }

    function stackBootstrap() {
      const backendGeneration = 'backend-generation-h10'
      const attachment = {
        attachmentId: 'tauri-attachment-h10',
        backendInstanceId: 'tauri-btleplug-h10',
        backendGeneration,
        adapter: {
          adapterId: 'tauri-adapter-h10',
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
          adapterGeneration: 'adapter-generation-h10',
          limitations: []
        }
      }
      const schemaRange = {
        axis: 'capability-schema',
        minimum: { axis: 'capability-schema', value: 1 },
        maximum: { axis: 'capability-schema', value: 1 }
      }
      const limitation = {
        code: 'deterministic-only',
        explanation: 'The fixture exposes deterministic host evidence only.',
        affectedGuarantee: 'Physical-radio qualification is not claimed.'
      }
      return {
        attachment,
        attachmentId: attachment.attachmentId,
        versions: {
          backendContract: negotiated('backend-contract'),
          capabilitySchema: negotiated('capability-schema'),
          eventSchema: negotiated('event-schema'),
          traceFormat: negotiated('trace-format'),
          ipcProtocol: negotiated('ipc-protocol', 4)
        },
        capabilities: {
          schemaVersion: 2,
          backendGeneration,
          descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
            id,
            state: 'limited',
            selectedSchemaRange: schemaRange,
            implementationOrigin: 'backend-native',
            tck: {
              suiteId: 'capability.catalog-v2',
              requiredScenarioIds: ['gatt.descriptor-discovery-read-write'],
              contractRange: schemaRange
            },
            evidence: {
              receiptId: `fixture-${id}`,
              evidenceLevel: 'deterministic',
              implementationVersion: 'fixture-v2',
              sourceDigest: `fixture-${id}`,
              scenarioIds: ['gatt.descriptor-discovery-read-write'],
              limitations: [limitation]
            },
            limitations: [limitation],
            limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
          }))
        },
        core: { contractRevision: 'C-UBM.0.1.2-DRAFT', implementationVersion: '5.0.0-rc.7' },
        renderer: { clientId: 'tauri-client-h10', windowScope: 'main', sessionScope: 'session-h10' },
        rendererLease: { leaseId: 'tauri-lease-h10', generation: 'tauri-lease-generation-h10' }
      }
    }

    let discoveries = 0
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: stackBootstrap() }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      const { command } = request.envelope
      if (command === 'connection.events.ready') {
        FakeChannel.current.emit({
          rendererLease: { leaseId: 'tauri-lease-h10', generation: 'tauri-lease-generation-h10' },
          eventId: 'lifecycle-event-1',
          streamId: 'connection-events-ipc-1',
          item: {
            kind: 'value',
            value: {
              kind: 'connection-lifecycle',
              schemaVersion: 2,
              attachment: stackBootstrap().attachment,
              attachmentId: 'tauri-attachment-h10',
              peerId: 'polar-h10',
              connectionId: 'connection-id-h10',
              connectionGeneration: 'generation-h10',
              ownerLeaseId: 'tauri-lease-h10',
              sequence: 1,
              backendIngressOrdinal: null,
              previous: 'connecting',
              current: 'connected',
              cause: 'connected'
            }
          }
        })
        return { kind: 'route', payload: { state: 'ready' } }
      }
      const responses = {
        'connection.connect': {
          handle: 'connection-h10',
          connectionId: 'connection-id-h10',
          ownerLeaseId: 'tauri-lease-h10',
          peerId: 'polar-h10',
          connectionGeneration: 'generation-h10'
        },
        'connection.events.subscribe': {
          handle: 'connection-events-ipc-1',
          connectionId: 'connection-id-h10',
          connectionGeneration: 'generation-h10',
          eventSchemaVersion: 2
        },
        'connection.events.unsubscribe': { state: 'released', failures: [] },
        'connection.disconnect': { state: 'released', failures: [] }
      }
      if (command === 'gatt.discover') {
        discoveries += 1
        return { kind: 'route', payload: h10WirePayload(false, `stack-${discoveries}`) }
      }
      if (responses[command] !== undefined) return { kind: 'route', payload: responses[command] }
      throw new Error(`unexpected route ${command}`)
    })

    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })
    const connection = await manager.connect('polar-h10')

    const first = await connection.discover()
    expect(first.snapshot().characteristics).toHaveLength(12)
    expect(first.service('180d').characteristic('2a37').descriptor(CCCD).occurrence).toBe(0)

    const second = await connection.discover()
    expect(second.snapshot().characteristics).toHaveLength(12)
    expect(second.snapshot().descriptors).toHaveLength(4)
    expect(second.service('180d').characteristic('2a37').properties.notify).toBe(true)

    let stale = null
    try {
      first.snapshot()
    } catch (error) {
      stale = error
    }
    expect(stale).toBeInstanceOf(BleError)
    expect(stale.code).toBe('gatt.stale-handle')

    await expect(connection.disconnect()).resolves.toMatchObject({ state: 'released' })
    await manager.destroy()
  })
})
