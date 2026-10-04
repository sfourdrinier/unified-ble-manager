const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { defaultPeripheral } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeManagerHost } = require('../../../src/react-native-manager')
const { composeReactNativePublicManager } = require('../../../src/react-native-public-manager')

const APPLE_UUID = '23288D29-3C2B-4D84-9000-000000000001'
const envelope = accessories => JSON.stringify({ revision: 'ubm-accessory-authorized/1', accessories })

async function managerFor(native, { askAvailable = true } = {}) {
  if (typeof native.authorizedAccessories === 'function') {
    native.chooseAccessory = jest.fn(async () => {
      throw new Error('picker must not open')
    })
    native.cancelAccessoryChoice = jest.fn(async () => {})
    native.accessoryChooserAvailable = jest.fn(async () => askAvailable)
  }
  const harness = rustCoreHarness({ platform: 'apple', native })
  const host = await createReactNativeManagerHost(environment(harness))
  return composeReactNativePublicManager(host, () => 1000)
}

test('saved ASK directory uses only actual OS-authorized UUIDs, not a scan or global Bluetooth grant', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  native.authorizedAccessories = jest.fn(async function () {
    expect(this).toBe(native)
    return envelope([{ bluetoothIdentifier: APPLE_UUID, name: 'AS label' }])
  })
  const manager = await managerFor(native)
  expect(manager.capabilities.get('peer:origin-authorized').state).toBe('limited')
  expect(manager.capabilities.get('peer:resolve-reference').state).toBe('limited')
  const peers = await manager.peers.authorized({ timeoutMs: 1000 })
  expect(peers).toHaveLength(1)
  expect(peers[0]).toMatchObject({
    name: 'AS label',
    rssi: null,
    sources: ['origin-authorized'],
    lastAdvertisement: null,
    state: { connection: 'unknown', reachability: 'unknown' },
    reference: { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_UUID }
  })
  expect(native.opsInvoked('scan.start')).toHaveLength(0)
  expect(native.opsInvoked('connection.connect')).toHaveLength(0)
  await manager.destroy()
})

test('fresh manager resolves only persisted origin UUID still in saved ASK list, then direct-connects without scan', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  native.peripherals.set(APPLE_UUID, defaultPeripheral(APPLE_UUID))
  native.authorizedAccessories = jest.fn(async () => envelope([{ bluetoothIdentifier: APPLE_UUID, name: 'OS label' }]))
  const manager = await managerFor(native)
  const reference = { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_UUID }
  const peer = await manager.peers.resolve(reference, { timeoutMs: 1000 })
  expect(peer?.reference).toEqual(reference)
  expect(peer?.state.connection).toBe('unknown')
  const connection = await manager.connect(reference, { timeoutMs: 1000 })
  expect(connection.peer.reference).toEqual(reference)
  expect(native.opsInvoked('scan.start')).toHaveLength(0)
  expect(native.opsInvoked('connection.connect')).toHaveLength(1)
  await connection.disconnect()
  await manager.destroy()
})

test('removed OS authorization does not resolve a persisted origin reference', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  native.authorizedAccessories = jest.fn(async () => envelope([]))
  const manager = await managerFor(native)
  const reference = { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_UUID }
  expect(await manager.peers.resolve(reference, { timeoutMs: 1000 })).toBeNull()
  await expect(manager.connect(reference, { timeoutMs: 1000 })).rejects.toMatchObject({ code: 'peer.not-found' })
  expect(native.opsInvoked('connection.connect')).toHaveLength(0)
  await manager.destroy()
})

test('old Apple binding refuses authorized category explicitly', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  const manager = await managerFor(native)
  expect(manager.capabilities.supports('peer:origin-authorized')).toBe(false)
  await expect(manager.peers.authorized()).rejects.toMatchObject({ code: 'capability.unsupported' })
  await manager.destroy()
})

test('old and undeclared Apple hosts preserve null resolve without querying ASK', async () => {
  const reference = { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_UUID }
  const { native: oldNative } = rustCoreHarness({ platform: 'apple' })
  const oldManager = await managerFor(oldNative)
  expect(await oldManager.peers.resolve(reference)).toBeNull()
  await oldManager.destroy()

  const { native: undeclaredNative } = rustCoreHarness({ platform: 'apple' })
  undeclaredNative.authorizedAccessories = jest.fn(async () =>
    envelope([{ bluetoothIdentifier: APPLE_UUID, name: 'not admitted' }])
  )
  const undeclaredManager = await managerFor(undeclaredNative, { askAvailable: false })
  expect(await undeclaredManager.peers.resolve(reference)).toBeNull()
  await expect(undeclaredManager.peers.authorized()).rejects.toMatchObject({ code: 'capability.unsupported' })
  expect(undeclaredNative.authorizedAccessories).not.toHaveBeenCalled()
  await undeclaredManager.destroy()
})

test('a supported ASK host preserves its native authorization-query failure during resolve', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  native.authorizedAccessories = jest.fn(async () => {
    throw new Error(
      JSON.stringify({
        code: 'platform.failure',
        domain: 'platform',
        operation: 'accessory.authorized',
        detail: 'session failed'
      })
    )
  })
  const manager = await managerFor(native)
  const reference = { version: 1, backendId: 'unified-ble:react-native-apple', scope: 'origin', opaqueId: APPLE_UUID }
  await expect(manager.peers.resolve(reference)).rejects.toMatchObject({ code: 'platform.failure' })
  await manager.destroy()
})

test('saved ASK query guards source and reference filters and refuses unsupported service claims', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  native.authorizedAccessories = jest.fn(async () => envelope([{ bluetoothIdentifier: APPLE_UUID, name: null }]))
  const manager = await managerFor(native)
  expect(await manager.peers.authorized({ sources: ['scan-observed'] })).toEqual([])
  expect(
    await manager.peers.authorized({
      references: [{ version: 1, backendId: 'other', scope: 'origin', opaqueId: APPLE_UUID }]
    })
  ).toEqual([])
  await expect(manager.peers.authorized({ services: ['180d'] })).rejects.toMatchObject({
    code: 'capability.unsupported'
  })
  await manager.destroy()
})

test('malformed or duplicate OS UUID list refuses instead of publishing a guessed identity', async () => {
  for (const text of [
    '{}',
    envelope([{ bluetoothIdentifier: 'bad', name: 'x' }]),
    envelope([
      { bluetoothIdentifier: APPLE_UUID, name: null },
      { bluetoothIdentifier: APPLE_UUID, name: null }
    ])
  ]) {
    const { native } = rustCoreHarness({ platform: 'apple' })
    native.authorizedAccessories = jest.fn(async () => text)
    const manager = await managerFor(native)
    await expect(manager.peers.authorized()).rejects.toMatchObject({ code: 'protocol.malformed' })
    await manager.destroy()
  }
})

test('a late ASK list cannot publish a peer after manager teardown', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  let complete
  native.authorizedAccessories = jest.fn(
    () =>
      new Promise(resolve => {
        complete = resolve
      })
  )
  const manager = await managerFor(native)
  const pending = manager.peers.authorized({ timeoutMs: 1000 })
  await manager.destroy()
  complete(envelope([{ bluetoothIdentifier: APPLE_UUID, name: 'late' }]))
  await expect(pending).rejects.toMatchObject({ code: 'lifecycle.destroyed' })
})

test('aborting the read-only ASK wait rejects without publishing a late list', async () => {
  const { native } = rustCoreHarness({ platform: 'apple' })
  let complete
  let entered
  const nativeEntered = new Promise(resolve => {
    entered = resolve
  })
  native.authorizedAccessories = jest.fn(
    () =>
      new Promise(resolve => {
        complete = resolve
        entered()
      })
  )
  const manager = await managerFor(native)
  const controller = new AbortController()
  const pending = manager.peers.authorized({ signal: controller.signal, timeoutMs: 1000 })
  await nativeEntered
  controller.abort()
  await expect(pending).rejects.toMatchObject({ code: 'operation.aborted' })
  complete(envelope([{ bluetoothIdentifier: APPLE_UUID, name: 'late' }]))
  expect(native.opsInvoked('scan.start')).toHaveLength(0)
  await manager.destroy()
})
