// __tests__/backends/reactnative/rust-core-public-link-controls.test.js
//
// The public `connection.controls` link surface over the React Native Rust
// route, driven through the ordinary factory, the real binding and codec, and
// the deterministic owner. Physical Android run against a Polar H10 threw a raw
// `TypeError` from `controls.effectiveMtu()`: the public layer invoked the
// connection's `effectiveMtu` detached from its receiver. Every control either
// answers or fails with a contract error; none escapes as a raw JavaScript error.

const { rustCoreHarness, environment, scanOptions } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

async function openConnection(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  const scan = await manager.scan(scanOptions())
  harness.native.emitAdvertisement()
  const advertisement = await scan.observations[Symbol.asyncIterator]().next()
  await scan.stop()
  const connection = await publicManager.connect(advertisement.value.value.device.id)
  return { manager, publicManager, connection }
}

async function failure(promise) {
  return promise.then(
    value => ({ resolved: value }),
    error => ({ name: error.name, code: error.code, limitations: (error.limitations ?? []).map(entry => entry.code) })
  )
}

describe('React Native Rust route: public link controls', () => {
  test('android: effectiveMtu answers before and after requestMtu instead of throwing a TypeError', async () => {
    const { manager, connection } = await openConnection('android')

    // No `onMtuChanged` yet: the platform reports no measurement.
    const before = await connection.controls.effectiveMtu()
    expect(before).toMatchObject({ state: 'unavailable', attMtu: null, payloadBytes: null, platformPduBytes: null })

    const negotiation = await connection.controls.requestMtu(517)
    expect(negotiation).toMatchObject({
      state: 'accepted',
      requestedMtu: 517,
      observation: { state: 'measured', attMtu: 247, payloadBytes: 244 }
    })

    const after = await connection.controls.effectiveMtu()
    expect(after).toMatchObject({ state: 'measured', attMtu: 247, payloadBytes: 244 })
    await manager.destroy()
  })

  test('android: maximumWriteLength answers the ATT default before an MTU exchange and the negotiated payload after', async () => {
    const { manager, connection } = await openConnection('android')
    const read = async mode => {
      const observation = await connection.controls.maximumWriteLength(mode)
      expect(observation).toMatchObject({ state: 'measured', mode, connectionGeneration: expect.any(String) })
      return observation.maximumWriteLength
    }
    expect(await read('with-response')).toBe(512)
    expect(await read('without-response')).toBe(20)
    await connection.controls.requestMtu(517)
    expect(await read('with-response')).toBe(512)
    expect(await read('without-response')).toBe(244)
    await manager.destroy()
  })

  test('android: parameters stays capability.unsupported', async () => {
    const { manager, connection } = await openConnection('android')
    expect(await failure(connection.controls.parameters())).toMatchObject({ code: 'capability.unsupported' })
    await manager.destroy()
  })

  test('apple: maximumWriteLength is CoreBluetooth maximumWriteValueLength(for:) per type', async () => {
    const { manager, connection } = await openConnection('apple')
    expect(await connection.controls.maximumWriteLength('with-response')).toMatchObject({
      state: 'measured',
      mode: 'with-response',
      maximumWriteLength: 512
    })
    expect(await connection.controls.maximumWriteLength('without-response')).toMatchObject({
      state: 'measured',
      mode: 'without-response',
      maximumWriteLength: 182
    })
    await manager.destroy()
  })

  test('apple: effectiveMtu fails closed as capability.unsupported with the CoreBluetooth reason', async () => {
    const { manager, connection } = await openConnection('apple')
    expect(await failure(connection.controls.effectiveMtu())).toEqual({
      name: expect.any(String),
      code: 'capability.unsupported',
      limitations: ['corebluetooth-effective-mtu-unavailable']
    })
    await manager.destroy()
  })

  test.each([
    [
      'android',
      [
        'android-att-default-mtu-before-exchange',
        'android-prepared-write-with-response',
        'live-radio-qualification-pending'
      ]
    ],
    ['apple', ['live-radio-qualification-pending']]
  ])('%s: gatt:maximum-write-length is registered limited with platform-named reasons', async (platform, codes) => {
    const { manager } = await openConnection(platform)
    const registration = manager.attachedBackend.backend.features.registrations.find(
      entry => entry.id === 'gatt:maximum-write-length'
    )
    expect(registration.state).toBe('limited')
    expect(registration.limitations.map(entry => entry.code)).toEqual(codes)
    expect(registration.limits.maximumWriteLength).toEqual({ minimum: 1, maximum: 512, unit: 'bytes' })
    await manager.destroy()
  })

  test('android: the registered implementation answers a discovered database path', async () => {
    const harness = rustCoreHarness({ platform: 'android' })
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const backend = manager.attachedBackend.backend
    const peerId = backend.connections.peerFromAddress({ address: 'A0:9E:1A:00:00:01', addressType: 'public' })
    const connection = await manager.connect(peerId, { signal: null, deadline: null })
    const database = await connection.discover({ signal: null, deadline: null })
    const path = (await database.snapshot()).characteristics[0].path
    await expect(database.maximumWriteLength(path, 'without-response')).resolves.toMatchObject({
      mode: 'without-response',
      maximumWriteLength: 20
    })
    await manager.destroy()
  })
})
