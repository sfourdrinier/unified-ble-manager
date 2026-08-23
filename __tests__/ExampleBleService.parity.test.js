// __tests__/ExampleBleService.parity.test.js

/**
 * Canonical Unified BLE 4.0 example parity and profile wiring guards.
 */
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const bare = fs.readFileSync(path.join(root, 'example/src/services/BLEService/BLEService.ts'), 'utf8')
const expo = fs.readFileSync(path.join(root, 'example-expo/src/services/BLEService/BLEService.ts'), 'utf8')

function assertCanonicalService(source, host = 'react-native') {
  expect(source).toContain(`from 'unified-ble-manager/${host}'`)
  expect(source).toContain(host === 'expo' ? 'createExpoBleManager' : 'createReactNativeBleManager')
  expect(source).toContain('class CanonicalBleExampleService')
  expect(source).toContain('async scanForPeers')
  expect(source).toContain('await manager.scan({')
  expect(source).toContain("duplicates: 'all'")
  expect(source).toContain('async connect(peer: ExamplePeer)')
  expect(source).toContain('connection.discover(this.operation())')
  expect(source).toContain('async readCharacteristic')
  expect(source).toContain('database.characteristic(serviceUuid, characteristicUuid).read')
  expect(source).toContain('async writeCharacteristic')
  expect(source).toContain('database.characteristic(serviceUuid, characteristicUuid).write')
  expect(source).toContain('async subscribeCharacteristic')
  expect(source).toContain('database.characteristic(serviceUuid, characteristicUuid).subscribe')
  expect(source).toContain('assertReleased(await subscription.remove()')
  expect(source).toContain('assertReleased(await manager.destroy()')
  expect(source).toContain("item.kind === 'overflow'")
  expect(source).toContain('AbortController')
  expect(source).toContain('this.destroying')
  expect(source).toMatch(/this\.destroying = false/)
  expect(source).toContain('await this.stopNotification()')
  expect(source).toContain('timeoutMs: 15_000')
  expect(source).toContain('instanceId:')
  expect(source).not.toContain('readHeartRateMeasurement')
  expect(source).not.toContain('TEMPERATURE_MEASUREMENT_CHARACTERISTIC')
  expect(source).not.toContain('BLOOD_PRESSURE_MEASUREMENT_CHARACTERISTIC')
  expect(source).not.toContain('signal: null, deadline: null')
  expect(source).not.toContain('hostSessionScope')
  expect(source).not.toContain('clientId:')
  expect(source).not.toContain('new BleManager(')
  expect(source).not.toContain('connectToDevice')
  expect(source).not.toContain('readCharacteristicForDevice')
  expect(source).not.toContain('writeCharacteristicWithResponseForDevice')
  expect(source).not.toContain('writeCharacteristicWithoutResponseForDevice')
  expect(source).not.toContain('readDescriptorForDevice')
  expect(source).not.toContain('writeDescriptorForDevice')
  expect(source).not.toContain('startDeviceScan')
  expect(source).not.toContain('cancelDeviceConnection')
}

function normalizeServiceSource(source) {
  return source
    .replace('// example-expo/', '// example/')
    .replace("from 'unified-ble-manager/expo'", "from 'unified-ble-manager/react-native'")
    .replaceAll('createExpoBleManager', 'createReactNativeBleManager')
    .replace('The Expo app owns', 'The bare app owns')
    .replace(
      '// This application identifier is stable across manager recreation and native restoration adoption.\n',
      ''
    )
    .replaceAll('EXPO_APPLICATION_BLE_CLIENT_ID', 'BARE_APPLICATION_BLE_CLIENT_ID')
    .replaceAll('EXPO_APPLICATION_HOST_SESSION_SCOPE', 'BARE_APPLICATION_HOST_SESSION_SCOPE')
    .replace(
      'let nextExampleManagerId = 1\n\nconst BARE_APPLICATION_BLE_CLIENT_ID',
      'let nextExampleManagerId = 1\nconst BARE_APPLICATION_BLE_CLIENT_ID'
    )
    .replaceAll('expo-example-', 'bare-example-')
    .replace('The Expo example does not', 'The bare example does not')
}

describe('canonical example BLEService parity (bare ↔ Expo)', () => {
  test('both examples expose the same canonical manager lifecycle and byte GATT surface', () => {
    assertCanonicalService(bare)
    assertCanonicalService(expo, 'expo')
    expect(normalizeServiceSource(expo)).toBe(bare)
  })

  test('async operation failures propagate or are explicitly surfaced after cleanup', () => {
    for (const source of [bare, expo]) {
      expect(source).not.toContain('new Promise<')
      expect(source).toMatch(
        /catch \(error\) \{[\s\S]*?this\.connection = connection[\s\S]*?await this\.disconnect\(\)[\s\S]*?throw error/
      )
      expect(source).toMatch(/readProfileValue[\s\S]*?gatt\.not-found[\s\S]*?gatt\.property-not-supported/)
      expect(source).toMatch(/consumeScan[\s\S]*?catch \(error\) \{[\s\S]*?console\.error/)
      expect(source).toMatch(/consumeNotification[\s\S]*?catch \(error\) \{[\s\S]*?console\.error/)
    }
  })

  test('both examples retain Battery and DIS reads and do not read indicate-only SIG measurements', () => {
    for (const source of [bare, expo]) {
      expect(source).toContain('async readCommonProfiles')
      expect(source).toContain('unified-ble-manager/profiles/battery-service')
      expect(source).toContain('unified-ble-manager/profiles/device-information')
      expect(source).toContain('parseBatteryLevel')
      expect(source).toContain('decodeDeviceInformationString')
      expect(source).toContain('readProfileValue')
      expect(source).toContain('readDeviceInformation')
      expect(source).not.toContain('unified-ble-manager/profiles/health-thermometer')
      expect(source).not.toContain('unified-ble-manager/profiles/blood-pressure')
      expect(source).not.toContain('isReadable === false')
    }
  })

  test('Dashboard and DeviceDetails keep canonical scan, connection, and profile controls wired', () => {
    for (const exampleDirectory of ['example', 'example-expo']) {
      const dashboard = fs.readFileSync(
        path.join(root, exampleDirectory, 'src/screens/MainStack/DashboardScreen/DashboardScreen.tsx'),
        'utf8'
      )
      expect(dashboard).toContain('BLEService.scanForPeers')
      expect(dashboard).toContain('BLEService.connect')
      expect(dashboard).not.toContain('BLEService.initializeBLE')
      expect(dashboard).not.toContain('BLEService.connectToDevice')

      const details = fs.readFileSync(
        path.join(root, exampleDirectory, 'src/screens/MainStack/DeviceDetailsScreen/DeviceDetailsScreen.tsx'),
        'utf8'
      )
      expect(details).toContain('BLEService.readCommonProfiles')
      expect(details).toContain('Read common profiles')
      expect(details).toContain("Platform.OS === 'android'")
      expect(details).toContain('ATT MTU requests are unavailable on Apple CoreBluetooth')
    }
  })
})
