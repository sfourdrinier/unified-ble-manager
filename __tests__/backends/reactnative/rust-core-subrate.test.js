const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const PEER = 'A0:9E:1A:00:00:01'

async function open(platform, available) {
  const h = rustCoreHarness({ platform, nativeOptions: { subrateAvailable: available } })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h, { androidApiLevel: 36 }))
  const manager = await createPublicBleManager(internal, () => 1000)
  const backend = internal.attachedBackend.backend
  const peer =
    platform === 'android'
      ? backend.connections.peerFromAddress({ address: PEER, addressType: 'public' })
      : backend.peerIdForNativeId(PEER)
  return { ...h, manager, connection: await manager.connect(peer) }
}

test('the public subrate route uses the native runtime probe and requests every allowed preset', async () => {
  const { native, manager, connection } = await open('android', true)
  try {
    expect(native.opsInvoked('connection.control-capabilities')).toHaveLength(1)
    for (const mode of ['default', 'low-latency', 'low-power', 'high-throughput']) {
      expect(await connection.controls.requestSubrate(mode)).toMatchObject({
        state: 'accepted',
        requested: mode,
        observation: null
      })
      expect(native.opsInvoked('connection.request-subrate').at(-1)).toMatchObject({ peerId: PEER, mode })
    }
    await expect(connection.controls.requestSubrate('system-update')).rejects.toMatchObject({
      code: 'argument.invalid'
    })
    expect(native.opsInvoked('connection.request-subrate')).toHaveLength(4)
    native.failNext(
      'connection.request-subrate',
      'permission.denied',
      'adapter',
      'connection.request-subrate',
      null,
      null,
      {
        domain: 'android',
        code: 'ERROR_MISSING_BLUETOOTH_CONNECT_PERMISSION',
        message: 'BLUETOOTH_CONNECT',
        metadata: { androidBluetoothStatus: 6, nativeDomain: 'android.bluetooth.BluetoothStatusCodes' }
      }
    )
    await expect(connection.controls.requestSubrate('low-power')).rejects.toMatchObject({
      code: 'permission.denied',
      platform: { domain: 'android', metadata: { androidBluetoothStatus: 6 } }
    })
  } finally {
    await connection.release()
    await manager.destroy()
  }
})

test.each([
  ['android', false],
  ['apple', true]
])('%s refuses unsupported subrate without native request effects', async (platform, available) => {
  const { native, manager, connection } = await open(platform, available)
  try {
    await expect(connection.controls.requestSubrate('default')).rejects.toMatchObject({
      code: 'capability.unsupported'
    })
    expect(native.opsInvoked('connection.request-subrate')).toHaveLength(0)
  } finally {
    await connection.release()
    await manager.destroy()
  }
})
