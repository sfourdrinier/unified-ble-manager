'use strict'

// Public factory -> desktop provider -> public characteristic over the documented
// injected binding. These are synthetic JavaScript facts, not N-API, OS flag
// collection, or physical-radio evidence. Expectations pin origin/main 5.2.1.
const path = require('node:path')

const SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const CHARACTERISTIC = '00002a37-0000-1000-8000-00805f9b34fb'
const peers = {
  corebluetooth: '00e2ce71-3ba4-6569-e3de-3081ce0c95fb',
  bluez: 'hci0/dev_AA_BB_CC_DD_EE_FF',
  winrt: 'public:AA:BB:CC:DD:EE:FF'
}
const clear = {
  secureRead: false,
  encryptRead: false,
  encryptAuthenticatedRead: false,
  secureWrite: false,
  encryptWrite: false,
  encryptAuthenticatedWrite: false,
  authorize: false
}
const legs = [
  ['corebluetooth', 'darwin', 'node-corebluetooth', 'createCoreBluetoothBleManager'],
  ['bluez', 'linux', 'node-bluez', 'createBluezBleManager'],
  ['winrt', 'win32', 'node-winrt', 'createWinRtBleManager']
]
const cases = [
  ['write with response alone', 0x02, clear, 'unknown', 'none'],
  ['write without response alone', 0x04, clear, 'unknown', 'none'],
  ['neither write property', 0x01, clear, 'none', 'unknown'],
  ['no read or write properties', 0x00, clear, 'unknown', 'unknown'],
  ['missing access', 0x07, undefined, 'unknown', 'unknown'],
  ['null access', 0x07, null, 'unknown', 'unknown'],
  ['empty access', 0x07, {}, 'unknown', 'unknown'],
  ['missing read flag', 0x07, { ...clear, secureRead: undefined }, 'unknown', 'none'],
  ['missing write flag', 0x07, { ...clear, secureWrite: undefined }, 'none', 'unknown'],
  ['null read flag', 0x07, { ...clear, encryptRead: null }, 'unknown', 'none'],
  ['null write flag', 0x07, { ...clear, encryptWrite: null }, 'none', 'unknown'],
  ['secure read', 0x07, { ...clear, secureRead: true }, 'encrypted', 'none'],
  ['encrypt read', 0x07, { ...clear, encryptRead: true }, 'encrypted', 'none'],
  ['authenticated read', 0x07, { ...clear, encryptAuthenticatedRead: true }, 'authenticated', 'none'],
  ['secure write', 0x07, { ...clear, secureWrite: true }, 'none', 'encrypted'],
  ['encrypt write', 0x07, { ...clear, encryptWrite: true }, 'none', 'encrypted'],
  ['authenticated write', 0x07, { ...clear, encryptAuthenticatedWrite: true }, 'none', 'authenticated'],
  ['authorize', 0x07, { ...clear, authorize: true }, 'authorized', 'authorized'],
  [
    'encrypted precedes authenticated and authorized',
    0x07,
    {
      ...clear,
      secureRead: true,
      encryptAuthenticatedRead: true,
      secureWrite: true,
      encryptAuthenticatedWrite: true,
      authorize: true
    },
    'encrypted',
    'encrypted'
  ],
  [
    'authenticated precedes authorized',
    0x07,
    {
      ...clear,
      encryptAuthenticatedRead: true,
      encryptAuthenticatedWrite: true,
      authorize: true
    },
    'authenticated',
    'authenticated'
  ]
]

function syntheticBinding(platform, bits, access) {
  const peerId = peers[platform]
  const record = { peerId, name: null, connection: 'disconnected' }
  const characteristic = {
    serviceUuid: SERVICE,
    serviceOccurrence: 0,
    characteristicUuid: CHARACTERISTIC,
    characteristicOccurrence: 0,
    properties: bits,
    ...(access === undefined ? {} : { access })
  }
  const central = {
    runtimeCapabilityStates: async () =>
      ['connection:direct', 'peer:known', 'peer:bonded'].map(id => ({ id, state: 'supported' })),
    awaitUsableAdapter: async () => {},
    adapterState: async () => 'powered-on',
    adapterAuthorization: async () => 'allowed',
    setEventWaker: () => {},
    takeLifecycleEvent: async () => null,
    takeSecurityEvent: async () => null,
    takeWriteReadinessEvent: async () => null,
    takeScanTerminalEvent: async () => null,
    takeAdapterResetEvent: async () => null,
    takeAdapterEvent: async () => null,
    createTicket: () => 'ticket',
    releaseTicket: () => {},
    resolvePeer: async () => record,
    bondedPeers: async () => [record],
    connect: async () => ({ peerKey: peerId, connectionGeneration: 'generation-1' }),
    discover: async () => ({
      pathsRegistered: 2,
      connectionGeneration: 'generation-1',
      databaseGeneration: 'database-1'
    }),
    discoveredPaths: async () => [{ serviceUuid: SERVICE, serviceOccurrence: 0, properties: 0 }, characteristic],
    disconnect: async ({ peerId, lease }) => ({
      schema: 'ubm-desktop-release/1',
      state: 'released',
      peerId,
      lease,
      connectionGeneration: 'generation-1'
    }),
    close: async () => ({ state: 'released', failures: [] })
  }
  return {
    listAdapters: async () => [{ index: 0, label: 'synthetic', default: true }],
    openProduction: async () => central
  }
}

async function openManager(os, module, factory, binding) {
  const original = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { configurable: true, value: os })
  try {
    return await require(path.join('..', '..', '..', 'src', module))[factory]({ binding })
  } finally {
    Object.defineProperty(process, 'platform', original)
  }
}

describe.each(legs)('%s public discovery access over injected binding', (platform, os, module, factory) => {
  test.each(cases)('%s', async (_name, bits, access, read, write) => {
    const manager = await openManager(os, module, factory, syntheticBinding(platform, bits, access))
    try {
      const backendId = {
        corebluetooth: 'unified-ble:corebluetooth',
        bluez: 'unified-ble:bluez-dbus',
        winrt: 'unified-ble:winrt'
      }[platform]
      const [peer] = await manager.peers.bonded()
      expect(peer.reference.backendId).toBe(backendId)
      const connection = await manager.connect(peer)
      const database = await connection.discover()
      const characteristic = database.characteristic(SERVICE, CHARACTERISTIC)
      expect(characteristic.properties.read).toBe((bits & 0x01) !== 0)
      expect(characteristic.properties.writeWithResponse).toBe((bits & 0x02) !== 0)
      expect(characteristic.properties.writeWithoutResponse).toBe((bits & 0x04) !== 0)
      expect(characteristic.access.read).toBe(read)
      expect(characteristic.access.write).toBe(write)
      expect(await connection.release()).toMatchObject({ state: 'released', failures: [] })
    } finally {
      expect(await manager.destroy()).toMatchObject({ state: 'released', failures: [] })
    }
  })
})
