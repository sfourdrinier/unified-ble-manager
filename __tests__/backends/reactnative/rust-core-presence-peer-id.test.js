// Finding 228: `presence.observe` took the public opaque peer id and passed it
// straight to the native side, which hands it to
// CompanionDeviceManager.startObservingDevicePresence — an API that requires a
// device address. Arming with the id the library itself hands out therefore
// failed on the phone with `platform.failure` ("android-peer-1-11 was not a
// valid MAC address"), while the raw address worked. Every other operation
// resolves the public id to the native one first; presence must too.

const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })
const ANDROID_ADDRESS = 'A0:9E:1A:E9:B9:3D'

async function androidBackend() {
  const harness = rustCoreHarness({ platform: 'android' })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  return { native: harness.native, manager, backend: manager.attachedBackend.backend }
}

describe('finding 228: presence observation resolves the public peer id', () => {
  test('observe and unobserve send the native address, not the opaque id', async () => {
    const { native, manager, backend } = await androidBackend()
    native.seedRestored([{ peerId: ANDROID_ADDRESS, name: null, connected: false }])
    const [peer] = await backend.peers.restored(NO_OPTIONS)
    // What a consumer holds is the opaque id, never the address.
    expect(String(peer.peerId)).not.toBe(ANDROID_ADDRESS)

    // The owner armed the ADDRESS, not the opaque id it handed the consumer.
    expect([...native.presenceArmed]).toEqual([])
    await backend.hostServices.observePresence({ peerId: String(peer.peerId) })
    expect([...native.presenceArmed]).toEqual([ANDROID_ADDRESS])
    await backend.hostServices.unobservePresence({ peerId: String(peer.peerId) })
    expect([...native.presenceArmed]).toEqual([])
    await manager.destroy()
  })

  test('a platform address still passes through unchanged (the native seam and the TCK use it)', async () => {
    const { native, manager, backend } = await androidBackend()
    await backend.hostServices.observePresence({ peerId: ANDROID_ADDRESS })
    expect([...native.presenceArmed]).toEqual([ANDROID_ADDRESS])
    await manager.destroy()
  })
})
