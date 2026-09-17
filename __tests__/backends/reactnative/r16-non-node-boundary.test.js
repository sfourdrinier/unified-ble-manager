// __tests__/backends/reactnative/r16-non-node-boundary.test.js
//
// R16 row "Non-Node boundary": with no Buffer global (Hermes/JSC), the
// supported wire forms decode correctly or fail structurally — never a
// bare ReferenceError.

let mockNativeModule = null

jest.mock('react-native', () => ({
  Platform: { OS: 'android', Version: 35 },
  TurboModuleRegistry: {
    get: () => mockNativeModule
  },
  NativeModules: {}
}))

const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native-manager')
const {
  RUST_CORE_CONTRACT_REVISION
} = require('../../../src/backends/reactnative/react-native-rust-core')

function record(overrides = {}) {
  return {
    ok: true,
    value: '{}',
    code: '',
    domain: '',
    operation: 'test.op',
    ...overrides
  }
}

const ZERO_COUNTERS = {
  activeScanControllers: 0,
  scanConsumers: 0,
  chooserSessions: 0,
  connectionLeases: 0,
  physicalLinks: 0,
  databaseSnapshots: 0,
  physicalCccdEnablements: 0,
  subscriptionConsumers: 0,
  queuedOperations: 0,
  dispatchedOperations: 0,
  retainedByteBuffers: 0,
  restorationRecords: 0,
  orphanedIpcOwners: 0
}

function presentNative(readValue) {
  mockNativeModule = {
    openSession: async () => ({ sessionId: 'sess-1' }),
    invoke: async (sessionId, op, argsJson) => {
      JSON.parse(argsJson)
      switch (op) {
        case 'adapter.state':
          return record({
            operation: op,
            value: JSON.stringify({
              availability: 'available',
              authorization: 'unknown',
              power: 'on',
              backendGeneration: 'gen-1',
              updatedAt: 123,
              safeReason: null
            })
          })
        case 'counters.describe':
          return record({ operation: op, value: JSON.stringify(ZERO_COUNTERS) })
        case 'events.take':
          return record({ operation: op, value: 'null' })
        case 'connection.connect':
          return record({
            operation: op,
            value: JSON.stringify({ peerKey: 'peerkey-1', connectionGeneration: 'conngen-1' })
          })
        case 'gatt.discover':
          return record({
            operation: op,
            value: JSON.stringify({
              services: [
                {
                  uuid: '0000180d-0000-1000-8000-00805f9b34fb',
                  occurrence: 0,
                  characteristics: [
                    {
                      uuid: '00002a37-0000-1000-8000-00805f9b34fb',
                      occurrence: 0,
                      properties: 9,
                      descriptors: []
                    }
                  ]
                }
              ]
            })
          })
        case 'gatt.read':
          return record({ operation: op, value: JSON.stringify({ value: readValue }) })
        case 'connection.disconnect':
        case 'session.dispose':
          return record({ operation: op, value: JSON.stringify({ state: 'released' }) })
        default:
          throw new Error(`unexpected native op ${op}`)
      }
    },
    close: async () => undefined,
    contractRevision: async () => RUST_CORE_CONTRACT_REVISION
  }
}

function environment() {
  return {
    platform: 'android',
    control: {},
    now: () => 1000,
    clientId: 'client-a',
    managerId: 'manager-a',
    hostSessionScope: 'scope-a'
  }
}

async function readMeasurement() {
  const manager = await createReactNativeBleManagerWithEnvironment(environment())
  try {
    const peerId = manager.attachedBackend.backend.connections.peerFromAddress({
      address: 'AA:BB:CC:DD:EE:FF',
      addressType: 'public'
    })
    const connection = await manager.connect(peerId, { signal: null, deadline: null })
    try {
      const database = await connection.discover({ signal: null, deadline: null })
      const snapshot = await database.snapshot()
      return await database.read(snapshot.characteristics[0].path, { signal: null, deadline: null })
    } finally {
      await connection.release()
    }
  } finally {
    await manager.destroy()
  }
}

function withoutBuffer() {
  const realBuffer = global.Buffer
  delete global.Buffer
  return () => {
    global.Buffer = realBuffer
  }
}

function errorCode(error) {
  return (error && error.normalized && error.normalized.code) || (error && error.code)
}

beforeEach(() => {
  mockNativeModule = null
})

describe('R16 non-Node boundary (no Buffer global)', () => {
  test('array wire form decodes without Buffer', async () => {
    presentNative([0x42])
    const restore = withoutBuffer()
    try {
      expect(typeof Buffer).toBe('undefined')
      const value = await readMeasurement()
      expect(value).toBeInstanceOf(Uint8Array)
      expect([...value]).toEqual([0x42])
    } finally {
      restore()
    }
  })

  test('base64 wire form fails structurally without Buffer (never ReferenceError)', async () => {
    presentNative({ base64: 'Qg==' })
    const restore = withoutBuffer()
    try {
      const error = await readMeasurement().then(
        () => null,
        failure => failure
      )
      expect(error).not.toBeNull()
      expect(error).not.toBeInstanceOf(ReferenceError)
      expect(errorCode(error)).toBe('protocol.malformed')
    } finally {
      restore()
    }
  })

  test('base64 wire form decodes with Buffer present (control)', async () => {
    presentNative({ base64: 'Qg==' })
    const value = await readMeasurement()
    expect([...value]).toEqual([0x42])
  })
})
