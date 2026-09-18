// __tests__/backends/desktop/desktop-legacy-identity.test.js
//
// The desktop Rust route names every identity as that host's own legacy
// backend did on origin/main: the attachment (instance, generations,
// attachment id), the listed adapter (display name, generations) and every
// resource (scan, peer, connection, lease, database, subscription).
//
// CoreBluetooth and BlueZ are compared with the real legacy providers over
// their in-memory boundaries; WinRT (no in-memory boundary for identity) with
// the formats origin/main `src/backends/winrt/winrt-backend.ts:450,587-600,
// 994-996,1164-1166,1474`, `winrt-gatt-operations.ts:698-699`,
// `winrt-subscription-runtime.ts:313` and `winrt-provider.ts:190-205` minted.

const h = require('../../helpers/desktop-rust-core-harness')
const { createCoreBluetoothBackendProvider } = require('../../../src/backends/corebluetooth/corebluetooth-provider')
const {
  InMemoryCoreBluetoothBoundary
} = require('../../../test-support/corebluetooth/in-memory-corebluetooth-boundary')
const {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
  BLUEZ_GATT_SERVICE_INTERFACE,
  InMemoryBluezBoundary,
  InMemoryBluezBoundaryFactory
} = require('../../../test-support/bluez/in-memory-bluez-object-manager')
const { createBluezBackendProvider } = require('../../../src/backends/bluez/bluez-backend-provider')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

/** Every identity a backend published over one scan -> connect -> discover -> subscribe run. */
async function identities(backend, { observe, subscribePath }) {
  const attachment = backend.identity.attachment
  const instance = String(attachment.backendInstanceId)
  const scan = await backend.scanner.start(h.scanOptions(), 'client-1')
  const iterator = scan.observations[Symbol.asyncIterator]()
  await observe()
  const observation = await h.nextValue(iterator, 5000)
  await iterator.return?.()
  await scan.stop()
  const lease = await backend.connections.connect(observation.device.id, 'client-1', NO_OPTIONS)
  const database = await backend.gatt.discover(lease.connection, NO_OPTIONS)
  const snapshot = await database.snapshot()
  const subscription = await backend.gatt.subscribe(subscribePath(snapshot), {
    operation: { ...NO_OPTIONS, correlation: 'identity-subscribe' },
    options: h.subscribeOptions()
  }).completion
  const named = {
    backendInstance: instance.replace(/-\d+$/, '-<n>'),
    attachmentId: String(attachment.attachmentId)
      .replace(instance, '<instance>')
      .replace(String(attachment.adapter.adapterId), '<adapter>'),
    backendGeneration: String(attachment.backendGeneration),
    adapterGeneration: String(attachment.adapter.adapterGeneration),
    stateGeneration: String(attachment.adapter.state.backendGeneration),
    scan: [String(scan.scanSessionId), String(scan.leaseId)],
    peer: String(observation.device.id),
    connection: [
      String(lease.connection.connectionId),
      String(lease.connection.connectionGeneration),
      String(lease.leaseId)
    ],
    database: [String(database.path.databaseId), String(database.path.databaseGeneration)],
    subscription: String(subscription.subscriptionId)
  }
  await backend.destroy()
  return named
}

async function listed(provider) {
  const [adapter] = await provider.listAdapters()
  return {
    adapterGeneration: String(adapter.adapterGeneration),
    stateGeneration: String(adapter.state.backendGeneration)
  }
}

async function rust(platform) {
  const opened = await h.openBackend(platform)
  const adapter = await listed(opened.provider)
  await opened.stage.stageServices('peer-1', h.hrmServices())
  const named = await identities(opened.backend, {
    observe: () => opened.stage.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Polar H10' }),
    subscribePath: snapshot =>
      snapshot.characteristics.find(entry => entry.path.characteristicUuid === h.HRM_MEASUREMENT).path
  })
  return { adapter, named, displayName: opened.backend.identity.attachment.adapter.displayName }
}

describe('desktop identities are each host’s legacy ones', () => {
  test('CoreBluetooth equals the legacy CoreBluetooth provider', async () => {
    let boundary
    const provider = createCoreBluetoothBackendProvider({
      boundaryFactory: () =>
        (boundary = new InMemoryCoreBluetoothBoundary({
          serviceUuid: h.HRM_SERVICE,
          characteristicUuid: h.HRM_MEASUREMENT
        })),
      now: () => 20,
      hostKind: 'node'
    })
    const adapter = await listed(provider)
    const [descriptor] = await provider.listAdapters()
    const backend = await provider.create({ selectedAdapterId: descriptor.adapterId })
    const expected = await identities(backend, {
      observe: async () => boundary.emitAdvertisement(),
      subscribePath: snapshot => snapshot.characteristics[0].path
    })
    const observed = await rust('corebluetooth')
    expect(observed.named).toEqual(expected)
    expect(observed.adapter).toEqual(adapter)
    expect(observed.displayName).toBe(descriptor.displayName)
  })

  test('BlueZ equals the legacy BlueZ provider', async () => {
    const adapterPath = '/org/bluez/hci0'
    const devicePath = `${adapterPath}/dev_AA_BB_CC_DD_EE_FF`
    const characteristicPath = `${devicePath}/service0001/char0001`
    const objects = () => [
      {
        path: adapterPath,
        interfaces: [
          {
            name: BLUEZ_ADAPTER_INTERFACE,
            properties: {
              Address: { signature: 's', value: '00:11:22:33:44:55' },
              Alias: { signature: 's', value: 'primary' },
              Powered: { signature: 'b', value: true }
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
              RSSI: { signature: 'n', value: -48 },
              UUIDs: { signature: 'as', value: [h.HRM_SERVICE] },
              Connected: { signature: 'b', value: true },
              ServicesResolved: { signature: 'b', value: true }
            }
          }
        ]
      },
      {
        path: `${devicePath}/service0001`,
        interfaces: [
          {
            name: BLUEZ_GATT_SERVICE_INTERFACE,
            properties: {
              Device: { signature: 'o', value: devicePath },
              UUID: { signature: 's', value: h.HRM_SERVICE },
              Primary: { signature: 'b', value: true }
            }
          }
        ]
      },
      {
        path: characteristicPath,
        interfaces: [
          {
            name: BLUEZ_GATT_CHARACTERISTIC_INTERFACE,
            properties: {
              Service: { signature: 'o', value: `${devicePath}/service0001` },
              UUID: { signature: 's', value: h.HRM_MEASUREMENT },
              Flags: { signature: 'as', value: ['read', 'notify'] }
            }
          }
        ]
      }
    ]
    const boundary = new InMemoryBluezBoundary({ objects: objects() })
    const provider = createBluezBackendProvider({
      busKind: 'system',
      boundaryFactory: new InMemoryBluezBoundaryFactory([new InMemoryBluezBoundary({ objects: objects() }), boundary]),
      now: () => 20
    })
    const adapter = await listed(provider)
    const backend = await provider.create({ selectedAdapterId: adapterPath })
    const expected = await identities(backend, {
      observe: async () =>
        boundary.objectManager.emitPropertiesChanged(devicePath, BLUEZ_DEVICE_INTERFACE, {
          RSSI: { signature: 'n', value: -30 }
        }),
      subscribePath: snapshot => snapshot.characteristics[0].path
    })
    const observed = await rust('bluez')
    expect(observed.named).toEqual(expected)
    expect(observed.adapter).toEqual(adapter)
  })

  test('WinRT uses the legacy WinRT formats', async () => {
    const observed = await rust('winrt')
    expect(observed.adapter).toEqual({ adapterGeneration: '1', stateGeneration: '1' })
    expect(observed.named).toEqual({
      backendInstance: 'winrt-backend-<n>',
      attachmentId: '<instance>:1:1',
      backendGeneration: '1',
      adapterGeneration: '1',
      stateGeneration: '1',
      scan: ['winrt-scan-session-1', 'winrt-scan-lease-1'],
      peer: 'winrt-peer-1',
      connection: ['winrt-connection-1', '1', 'winrt-connection-lease-1'],
      database: ['winrt-database-1', 'winrt-database-generation-1'],
      subscription: 'winrt-subscription-1'
    })
  })

  test.each(['corebluetooth', 'winrt', 'bluez'])('%s names no Rust-route vocabulary', async platform => {
    const observed = await rust(platform)
    expect(JSON.stringify(observed)).not.toMatch(/rust-core|desktop-|-core-|cg-\d|db-\d/)
  })
})
