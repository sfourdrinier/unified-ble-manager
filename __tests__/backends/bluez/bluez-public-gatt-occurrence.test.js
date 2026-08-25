// __tests__/backends/bluez/bluez-public-gatt-occurrence.test.js

const { attachBackend } = require('../../../src/backend-contract/backend')
const { capacity, opaqueId, version, versionRange } = require('../../../src/backend-contract/primitives')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')
const { createPublicGattDatabase } = require('../../../src/public/gatt')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
  BLUEZ_GATT_DESCRIPTOR_INTERFACE,
  BLUEZ_GATT_SERVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')

const adapterPath = '/org/bluez/hci0'
const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
const service0Path = `${devicePath}/service0001`
const service1Path = `${devicePath}/service0002`
const characteristic0Path = `${service0Path}/char0001`
const characteristic1Path = `${service1Path}/char0001`
const descriptor0Path = `${characteristic0Path}/desc0001`
const serviceUuid = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicUuid = '00002a37-0000-1000-8000-00805f9b34fb'
const descriptorUuid = '00002902-0000-1000-8000-00805f9b34fb'

function compatibility() {
  return {
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  }
}

function delivery() {
  return {
    itemCapacity: capacity(4),
    byteCapacity: capacity(4096),
    reservedControlCapacity: capacity(1),
    overflowPolicy: 'drop-oldest'
  }
}

function operation(signal = null) {
  return { signal, deadline: null }
}

function scanOptions() {
  return {
    filter: { serviceUuids: [serviceUuid], manufacturerData: [], localNamePrefix: 'Polar' },
    duplicatePolicy: 'all',
    timestampPolicy: 'receipt-monotonic',
    delivery: delivery(),
    deadline: null,
    signal: null,
    sharing: { mode: 'owner', allowSharing: true }
  }
}

function serviceObject(path) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_SERVICE_INTERFACE,
        properties: {
          Device: { signature: 'o', value: devicePath },
          UUID: { signature: 's', value: serviceUuid },
          Primary: { signature: 'b', value: true }
        }
      }
    ]
  }
}

function characteristicObject(path, service) {
  return {
    path,
    interfaces: [
      {
        name: BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
        properties: {
          Service: { signature: 'o', value: service },
          UUID: { signature: 's', value: characteristicUuid },
          Flags: { signature: 'as', value: ['read', 'write', 'notify'] },
          Value: { signature: 'ay', value: new Uint8Array([1]) },
          Notifying: { signature: 'b', value: false }
        }
      }
    ]
  }
}

function managedObjects() {
  return [
    {
      path: adapterPath,
      interfaces: [
        {
          name: BLUEZ_ADAPTER_INTERFACE,
          properties: {
            Address: { signature: 's', value: '00:11:22:33:44:55' },
            Alias: { signature: 's', value: 'primary' },
            Powered: { signature: 'b', value: true },
            Discovering: { signature: 'b', value: false }
          }
        }
      ]
    },
    {
      path: devicePath,
      interfaces: [
        {
          name: BLUEZ_DEVICE_INTERFACE,
          properties: {
            Address: { signature: 's', value: 'AA:BB:CC:DD:EE:FF' },
            AddressType: { signature: 's', value: 'random' },
            Alias: { signature: 's', value: 'Polar H10' },
            RSSI: { signature: 'n', value: -48 },
            UUIDs: { signature: 'as', value: [serviceUuid] },
            Connected: { signature: 'b', value: true },
            ServicesResolved: { signature: 'b', value: true }
          }
        }
      ]
    },
    serviceObject(service0Path),
    serviceObject(service1Path),
    characteristicObject(characteristic0Path, service0Path),
    characteristicObject(characteristic1Path, service1Path),
    {
      path: descriptor0Path,
      interfaces: [
        {
          name: BLUEZ_GATT_DESCRIPTOR_INTERFACE,
          properties: {
            Characteristic: { signature: 'o', value: characteristic0Path },
            UUID: { signature: 's', value: descriptorUuid }
          }
        }
      ]
    }
  ]
}

async function backendFixture() {
  const boundary = new InMemoryBluezBoundary({ objects: managedObjects() })
  const provider = createBluezBackendProvider({
    busKind: 'system',
    boundaryFactory: new InMemoryBluezBoundaryFactory([boundary]),
    now: () => 20
  })
  const backend = await provider.create({ selectedAdapterId: adapterPath })
  await attachBackend(backend, compatibility())
  return { backend, boundary }
}

async function observedPeerId(backend) {
  const scan = await backend.scanner.start(scanOptions(), opaqueId('peer-observer', 'client', 'bluez:public-gatt'))
  const observation = await scan.observations[Symbol.asyncIterator]().next()
  await scan.stop()
  if (observation.done || observation.value.kind !== 'value') {
    throw new Error('BlueZ test fixture did not emit a peer observation')
  }
  return observation.value.value.device.id
}

async function connectedDatabase(backend) {
  const peerId = await observedPeerId(backend)
  const lease = await backend.connections.connect(peerId, opaqueId('client-1', 'client', 'bluez:public-gatt'), operation())
  const database = await backend.gatt.discover(lease.connection, operation())
  return { lease, database }
}

function publicSourceFromBluezDatabase(database) {
  return {
    path: database.path,
    monotonicNow: () => 20,
    scheduleDeadline: () => ({ cancel() {} }),
    assertCurrent: () => database.assertCurrent('bluez.gatt.public-source'),
    snapshot: () => database.snapshot(),
    read: (path, options) => database.read(path, options),
    write: (path, bytes, options) => database.write(path, bytes, options),
    maximumWriteLength: async () => ({
      maximumWriteLength: 20,
      observedAtMonotonicMs: 20,
      mode: 'with-response'
    }),
    writeLong: async () => {
      throw new Error('writeLong is not exercised by the BlueZ public occurrence tests')
    },
    readDescriptor: (path, options) => database.readDescriptor(path, options),
    writeDescriptor: (path, bytes, options) => database.writeDescriptor(path, bytes, options),
    subscribe: (path, options) => database.subscribe(path, options)
  }
}

function withObjectPathOccurrences(snapshot) {
  return {
    ...snapshot,
    services: snapshot.services.map((service, index) => ({
      ...service,
      path: {
        ...service.path,
        serviceOccurrence: index === 0 ? service0Path : service1Path
      }
    })),
    characteristics: snapshot.characteristics.map((characteristic, index) => ({
      ...characteristic,
      path: {
        ...characteristic.path,
        serviceOccurrence: index === 0 ? service0Path : service1Path,
        characteristicOccurrence: index === 0 ? characteristic0Path : characteristic1Path
      }
    })),
    descriptors: snapshot.descriptors.map(descriptor => ({
      ...descriptor,
      path: {
        ...descriptor.path,
        serviceOccurrence: service0Path,
        characteristicOccurrence: characteristic0Path,
        descriptorOccurrence: descriptor0Path
      }
    }))
  }
}

function expectDecimalOccurrence(value) {
  expect(String(value)).toMatch(/^(0|[1-9][0-9]*)$/)
  expect(String(value)).not.toContain('/')
}

describe('BlueZ public GATT occurrences', () => {
  test('createPublicGattDatabase accepts a BlueZ snapshot and addresses duplicate UUID services', async () => {
    const { backend, boundary } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const snapshot = await database.snapshot()
    expect(snapshot.services).toHaveLength(2)
    expect(snapshot.services[0].path.serviceUuid).toBe(snapshot.services[1].path.serviceUuid)

    boundary.onCall(characteristic0Path, BLUEZ_GATT_CHARACTERISTIC_INTERFACE, 'ReadValue', async () => new Uint8Array([12, 13]))
    boundary.onCall(characteristic1Path, BLUEZ_GATT_CHARACTERISTIC_INTERFACE, 'ReadValue', async () => new Uint8Array([22, 23]))

    const publicDatabase = await createPublicGattDatabase(publicSourceFromBluezDatabase(database))
    expectDecimalOccurrence(snapshot.services[0].path.serviceOccurrence)
    expectDecimalOccurrence(snapshot.services[1].path.serviceOccurrence)
    expect(String(snapshot.services[0].path.serviceOccurrence)).toBe('0')
    expect(String(snapshot.services[1].path.serviceOccurrence)).toBe('1')
    expectDecimalOccurrence(snapshot.characteristics[0].path.characteristicOccurrence)
    expectDecimalOccurrence(snapshot.characteristics[1].path.characteristicOccurrence)
    expect(String(snapshot.characteristics[0].path.characteristicOccurrence)).toBe('0')
    expect(String(snapshot.characteristics[1].path.characteristicOccurrence)).toBe('0')
    expectDecimalOccurrence(snapshot.descriptors[0].path.descriptorOccurrence)

    expect(publicDatabase.servicesByUuid(serviceUuid).map(service => service.occurrence)).toEqual([0, 1])
    expect(() => publicDatabase.service(serviceUuid)).toThrow(
      expect.objectContaining({ code: 'gatt.ambiguous-path' })
    )
    const first = publicDatabase.service(serviceUuid, { occurrence: 0 })
    const second = publicDatabase.service(serviceUuid, { occurrence: 1 })
    expect(first.occurrence).toBe(0)
    expect(second.occurrence).toBe(1)
    expect(first.uuid).toBe(serviceUuid)
    expect(second.uuid).toBe(serviceUuid)

    await expect(first.characteristic(characteristicUuid).read()).resolves.toEqual(new Uint8Array([12, 13]))
    await expect(second.characteristic(characteristicUuid).read()).resolves.toEqual(new Uint8Array([22, 23]))
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })

  test('createPublicGattDatabase still rejects D-Bus object paths used as occurrences', async () => {
    const { backend } = await backendFixture()
    const { database } = await connectedDatabase(backend)
    const snapshot = withObjectPathOccurrences(await database.snapshot())
    const source = publicSourceFromBluezDatabase(database)
    source.snapshot = async () => snapshot

    await expect(createPublicGattDatabase(source)).rejects.toMatchObject({
      code: 'protocol.violation',
      operation: 'public-gatt.occurrence'
    })
    await expect(backend.destroy()).resolves.toEqual({ state: 'released', failures: [] })
  })
})
