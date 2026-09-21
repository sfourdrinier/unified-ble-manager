// __tests__/backends/reactnative/rust-core-android-resource-id.test.js
//
// Finding 223 (twin): FXF renamed Android peer ids to `android-peer-*`, but
// scan sessions, connections, GATT databases and subscriptions still minted
// `corebluetooth-*` on Android — naming a framework that is not on the
// device. Every resource id must name the platform that produced it, keep its
// shape and per-session stability. Apple keeps `corebluetooth-*`.

const {
  rustCoreHarness,
  environment,
  scanOptions,
  subscribeOptions
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

async function firstCycle(platform) {
  const { native, manager } = await rustManager(platform)
  const scan = await manager.scan(scanOptions())
  native.emitAdvertisement()
  const observation = (await take(scan.observations)).value.value
  const peerId = observation.device.id
  const scanSessionId = String(scan.scanSessionId)
  const scanLeaseId = String(scan.leaseId)
  await scan.stop()
  const connection = await manager.connect(peerId, NO_OPTIONS)
  const database = await connection.discover(NO_OPTIONS)
  const path = (await database.snapshot()).characteristics[0].path
  const subscription = await database.subscribe(path, subscribeOptions())
  const record = {
    scanSessionId,
    scanLeaseId,
    peerId: String(peerId),
    connectionId: String(connection.connectionId),
    connectionLeaseId: String(connection.ownerLeaseId),
    connectionGeneration: String(connection.connectionGeneration),
    databaseId: String(database.path.databaseId),
    databaseGeneration: String(database.path.databaseGeneration),
    subscriptionId: String(subscription.subscriptionId)
  }
  await subscription.remove()
  await connection.disconnect()
  await manager.destroy()
  return record
}

describe('finding 223 twin: Android resource ids name their platform', () => {
  test('android scan, connection, database and subscription carry android-*', async () => {
    expect(await firstCycle('android')).toEqual({
      scanSessionId: 'android-scan-session-1',
      scanLeaseId: 'android-scan-lease-1',
      peerId: 'android-peer-1-1',
      connectionId: 'android-connection-1',
      connectionLeaseId: 'android-connection-lease-1',
      connectionGeneration: 'android-connection-generation-1',
      databaseId: 'android-database-1',
      databaseGeneration: 'android-database-generation-1',
      subscriptionId: 'android-subscription-1'
    })
  })

  test('apple keeps corebluetooth-* everywhere', async () => {
    expect(await firstCycle('apple')).toEqual({
      scanSessionId: 'corebluetooth-scan-session-1',
      scanLeaseId: 'corebluetooth-scan-lease-1',
      peerId: 'corebluetooth-peer-1-1',
      connectionId: 'corebluetooth-connection-1',
      connectionLeaseId: 'corebluetooth-connection-lease-1',
      connectionGeneration: 'corebluetooth-connection-generation-1',
      databaseId: 'corebluetooth-database-1',
      databaseGeneration: 'corebluetooth-database-generation-1',
      subscriptionId: 'corebluetooth-subscription-1'
    })
  })
})
