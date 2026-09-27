const { createHeadlessContinuationJob } = require('../../../example-expo/src/driver/headless-continuation-job.ts')
const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { defaultPeripheral, DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

test('the actual headless reference job directly reads the known Android address on a fresh manager without scanning', async () => {
  const peripheral = defaultPeripheral()
  peripheral.services = peripheral.services.slice(0, 2)
  peripheral.services[1].characteristics = peripheral.services[1].characteristics.slice(0, 1)
  const harness = rustCoreHarness({ nativeOptions: { peripherals: [peripheral] } })
  const evidence = []
  const job = createHeadlessContinuationJob({
    createManager: async () =>
      createPublicBleManager(await createReactNativeBleManagerWithEnvironment(environment(harness)), () => 1000),
    save: async value => {
      evidence.push(value)
    },
    now: () => 1000,
    scheduleDeadline: () => () => {}
  })
  await job({ peerId: DEFAULT_PEER, event: 'companion.appeared' })
  expect(evidence.at(-1)).toMatchObject({
    state: 'completed',
    batteryPercent: 80,
    cleanup: { state: 'released', failures: [] }
  })
  expect(harness.native.opsInvoked('scan.start')).toHaveLength(0)
  expect(harness.native.opsInvoked('connection.connect')).toEqual([
    expect.objectContaining({ peerId: DEFAULT_PEER, intent: 'direct' })
  ])
  expect(harness.native.opsInvoked('gatt.read')).toHaveLength(1)
  expect(harness.native.opsInvoked('connection.disconnect')).toHaveLength(1)
  expect(harness.native.opsInvoked('session.dispose')).toHaveLength(1)
})
