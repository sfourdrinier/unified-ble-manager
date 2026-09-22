// __tests__/backends/reactnative/rust-core-restored-peer-name.test.js
//
// FXL finding 233: a restored peer carries an address but no name. The
// decision pinned here: `name: null` on a restored peer is CORRECT — the
// library has no advertisement observation for the peer in this process, so
// there is no observed name to report. The Companion Device Manager
// association display name ("Polar H10 E9B93D29" in `dumpsys
// companiondevice`) is a real platform fact but NOT an advertisement
// observation, so it is never silently merged into `name`.
//
// Parity: Apple `willRestoreState` hands back CBPeripherals that carry a
// name (`OwnedCoreBluetoothProtocolRadioSupport.restoredPeerSnapshots`
// reads `peripheral.name`), while Android presence wake hands back a bare
// address (`PresenceWakeCoordinator` ingests
// `PresenceRestoredPeer(address, null, false)`). `name` therefore means the
// same thing on both platforms — the name the OS handed back with the
// restoration, or null when it handed back none — and the platform that
// cannot answer says so with null rather than a plausible substitute.

const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const {
  createReactNativeBleManagerWithEnvironment
} = require('../../../src/react-native-manager')
const { createPublicPeerDirectory } = require('../../../src/public/peer-directory')

const NO_OPTIONS = Object.freeze({ signal: null, deadline: null })
const ANDROID_ADDRESS = 'A0:9E:1A:E9:B9:3D'
const APPLE_PEER = 'C0FFEE00-0000-4000-8000-000000000001'
const APPLE_NAME = 'Polar H10 E9B93D29'

async function rustBackend(platform) {
  const harness = rustCoreHarness({ platform })
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  return { native: harness.native, manager, backend: manager.attachedBackend.backend }
}

describe('FXL-233: a restored peer carries an address but no name', () => {
  test('Android presence wake restores a nameless peer: null name, null rssi, unknown state, address in the reference', async () => {
    const { native, manager, backend } = await rustBackend('android')
    // Models the OS wake after process death: CDM presence delivers only the
    // associated address, never an advertisement, so the owner holds no name.
    native.seedRestored([{ peerId: ANDROID_ADDRESS, name: null, connected: false }])
    const peers = await backend.peers.restored(NO_OPTIONS)
    expect(peers).toHaveLength(1)
    expect(peers[0]).toMatchObject({
      name: null,
      rssi: null,
      source: 'restored',
      state: {
        reachability: 'unknown',
        connection: 'unknown',
        bond: 'unknown',
        lastSeenAtMonotonicMs: null
      }
    })
    // The consumer CAN display the address: the origin reference carries it.
    expect(peers[0].reference).toMatchObject({ scope: 'origin', opaqueId: ANDROID_ADDRESS })
    await manager.destroy()
  })

  test('Apple restoration passes the OS-supplied peripheral name through verbatim', async () => {
    const { native, manager, backend } = await rustBackend('apple')
    native.seedRestored([{ peerId: APPLE_PEER, name: APPLE_NAME, connected: false }])
    const peers = await backend.peers.restored(NO_OPTIONS)
    expect(peers).toHaveLength(1)
    expect(peers[0].name).toBe(APPLE_NAME)
    expect(peers[0].source).toBe('restored')
    await manager.destroy()
  })

  test('the public restored peer reports null name, null advertisement, and the address-bearing reference', async () => {
    const record = Object.freeze({
      reference: Object.freeze({
        version: 1,
        backendId: 'unified-ble:react-native-android',
        scope: 'origin',
        opaqueId: ANDROID_ADDRESS
      }),
      peerId: 'android-peer-1-1',
      name: null,
      rssi: null,
      source: 'restored',
      state: Object.freeze({
        reachability: 'unknown',
        connection: 'unknown',
        bond: 'unknown',
        lastSeenAtMonotonicMs: null
      })
    })
    const directory = createPublicPeerDirectory(
      {
        resolve: async () => record,
        known: async () => [record],
        connected: async () => [],
        bonded: async () => [],
        authorized: async () => [],
        restored: async () => [record]
      },
      () => 1000
    )
    const peers = await directory.restored()
    expect(peers).toHaveLength(1)
    expect(peers[0]).toMatchObject({
      id: 'android-peer-1-1',
      name: null,
      rssi: null,
      sources: ['restored'],
      lastAdvertisement: null
    })
    expect(peers[0].reference).toMatchObject({ scope: 'origin', opaqueId: ANDROID_ADDRESS })
    expect(peers[0].state).toMatchObject({
      reachability: 'unknown',
      connection: 'unknown',
      bond: 'unknown',
      lastSeenAtMonotonicMs: null
    })
  })
})
