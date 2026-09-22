// __tests__/backends/reactnative/r16-non-node-boundary.test.js
//
// R16 row "Non-Node boundary": with no Buffer, atob or btoa global
// (Hermes/JSC), the production serializer encodes and decodes every byte form
// correctly, and malformed bytes fail structurally — never a bare
// ReferenceError. The deterministic module keeps its own Node Buffer
// reference, so only the package code runs without the globals.

const {
  rustCoreHarness,
  environment,
  subscribeOptions,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')

const removed = {}

beforeAll(() => {
  for (const name of ['Buffer', 'atob', 'btoa']) {
    removed[name] = Object.getOwnPropertyDescriptor(globalThis, name)
    delete globalThis[name]
  }
})

afterAll(() => {
  for (const [name, descriptor] of Object.entries(removed)) {
    if (descriptor !== undefined) Object.defineProperty(globalThis, name, descriptor)
  }
})

const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function opened() {
  const harness = rustCoreHarness({ platform: 'android' })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const connection = await manager.connect(
    backend.connections.peerFromAddress({ address: DEFAULT_PEER, addressType: 'public' }),
    NO_OPTIONS
  )
  const database = await connection.discover(NO_OPTIONS)
  const path = (await database.snapshot()).characteristics[0].path
  return { native: harness.native, manager, database, path }
}

describe('R16 non-Node boundary', () => {
  test('the globals are really gone', () => {
    expect(typeof Buffer).toBe('undefined')
    expect(typeof atob).toBe('undefined')
  })

  test('write, read and notification bytes round-trip without Buffer', async () => {
    const { native, manager, database, path } = await opened()
    await database.write(path, new Uint8Array([0x00, 0x80, 0xff]), { ...NO_OPTIONS, mode: 'with-response' })
    expect([...(await database.read(path, NO_OPTIONS))]).toEqual([0x00, 0x80, 0xff])
    const subscription = await database.subscribe(path, subscribeOptions())
    native.emitNotification(new Uint8Array([0xff, 0x00]))
    const item = await subscription.values[Symbol.asyncIterator]().next()
    expect([...item.value.value.value]).toEqual([0xff, 0x00])
    await manager.destroy()
  })

  test('malformed base64 fails structurally (protocol.malformed), never ReferenceError', async () => {
    const { native, manager, database, path } = await opened()
    native.hold('gatt.read')
    const read = database.read(path, NO_OPTIONS)
    await settle()
    native.release('gatt.read', { valueB64: 'not base64!' })
    await expect(read).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
    await manager.destroy()
  })
})
