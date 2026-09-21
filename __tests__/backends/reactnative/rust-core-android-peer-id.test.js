// __tests__/backends/reactnative/rust-core-android-peer-id.test.js
//
// Finding 223: the Android host reported peer ids prefixed
// `corebluetooth-peer-N`, leaking a CoreBluetooth identity onto Android. Peer
// ids must name the platform that produced them (`android-peer-{gen}-{n}` on
// Android, `corebluetooth-peer-{gen}-{n}` on Apple) while keeping the
// established id shape and stability within a session.

const {
  rustCoreHarness,
  environment,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })

async function rustManager(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness, {}))
  return { native: harness.native, manager }
}

async function take(stream) {
  return stream[Symbol.asyncIterator]().next()
}

async function firstPeerId(platform) {
  const { native, manager } = await rustManager(platform)
  const scan = await manager.scan(scanOptions())
  native.emitAdvertisement()
  const observation = (await take(scan.observations)).value.value
  const peerId = String(observation.device.id)
  await manager.destroy()
  return peerId
}

describe('finding 223: peer ids name the platform that produced them', () => {
  test('android peer ids carry the android-peer prefix', async () => {
    expect(await firstPeerId('android')).toBe('android-peer-1-1')
  })

  test('apple peer ids keep the corebluetooth-peer prefix', async () => {
    expect(await firstPeerId('apple')).toBe('corebluetooth-peer-1-1')
  })

  test('android peer ids are stable within a session', async () => {
    const { native, manager } = await rustManager('android')
    const scan = await manager.scan(scanOptions())
    native.emitAdvertisement()
    const first = String((await take(scan.observations)).value.value.device.id)
    native.emitAdvertisement()
    const second = String((await take(scan.observations)).value.value.device.id)
    expect(second).toBe(first)
    expect(first).toBe('android-peer-1-1')
    await manager.destroy()
  })
})
