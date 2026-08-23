// __tests__/backends/corebluetooth/corebluetooth-native-descriptor-source.test.js

const fs = require('fs')
const path = require('path')

const repositoryRoot = path.resolve(__dirname, '../../..')
const addonSource = fs.readFileSync(path.join(repositoryRoot, 'native/electron/corebluetooth/src/addon.mm'), 'utf8')
const bridgeSource = fs.readFileSync(path.join(repositoryRoot, 'native/electron/corebluetooth/index.js'), 'utf8')

describe('CoreBluetooth native descriptor boundary source', () => {
  test('discovers descriptors and exposes occurrence-addressed owned-byte read/write methods', () => {
    expect(addonSource).toContain('[peripheral discoverDescriptorsForCharacteristic:characteristic]')
    expect(addonSource).toContain('didDiscoverDescriptorsForCharacteristic')
    expect(addonSource).toContain('readValueForDescriptor:descriptor')
    expect(addonSource).toContain('writeValue:value forDescriptor:descriptor')
    expect(addonSource).toContain('InstanceMethod("readDescriptorAt", &CoreBluetoothAddon::ReadDescriptorAt)')
    expect(addonSource).toContain('InstanceMethod("writeDescriptorAt", &CoreBluetoothAddon::WriteDescriptorAt)')
    expect(addonSource).toContain('DescriptorReadBytes')
    expect(addonSource).toContain('DescriptorWriteValue')
    expect(bridgeSource).toContain("'readDescriptorAt'")
    expect(bridgeSource).toContain("'writeDescriptorAt'")
    expect(bridgeSource).toContain('descriptorOperationsAvailable: true')
    expect(bridgeSource).toContain('readDescriptor: address =>')
    expect(bridgeSource).toContain('writeDescriptor: (address, bytes) =>')
  })

  test('settles occurrence-addressed notification disable only from the CoreBluetooth state callback', () => {
    expect(addonSource).toContain('pendingNotifyDisableAt')
    const radioImplementation = addonSource.indexOf('@implementation UBMRadio')
    const stopNotifyAtStart = addonSource.indexOf('- (void)stopNotifyAt:', radioImplementation)
    const stopNotifyAt = addonSource.slice(
      stopNotifyAtStart,
      addonSource.indexOf('- (void)centralManagerDidUpdateState:', stopNotifyAtStart)
    )
    expect(stopNotifyAt).toContain('self.pendingNotifyDisableAt[key] = completion;')
    expect(stopNotifyAt).toContain('if (!peripheral)')

    const notificationStateCallback = addonSource.slice(
      addonSource.indexOf('didUpdateNotificationStateForCharacteristic:'),
      addonSource.indexOf('\n@end', addonSource.indexOf('didUpdateNotificationStateForCharacteristic:'))
    )
    expect(notificationStateCallback).toContain('UBMVoidBlock directDisableDone = self.pendingNotifyDisableAt[directKey];')
    expect(notificationStateCallback).toContain('directDisableDone(error);')
  })
})
