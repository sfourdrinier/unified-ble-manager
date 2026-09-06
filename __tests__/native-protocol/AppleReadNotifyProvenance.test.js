// __tests__/native-protocol/AppleReadNotifyProvenance.test.js

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
}

function sliceBetween(source, startMarker, endMarker) {
  const start = source.indexOf(startMarker)
  const end = source.indexOf(endMarker, start + startMarker.length)
  if (start < 0 || end < 0 || end <= start) {
    throw new Error(`Failed to slice source between ${JSON.stringify(startMarker)} and ${JSON.stringify(endMarker)}`)
  }
  return source.slice(start, end)
}

describe('CoreBluetooth read vs notify provenance (BLE-13)', () => {
  const radio = read('ios/Owned/OwnedCoreBluetoothProtocolRadio.swift')
  const cancellation = read('ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift')
  const support = read('ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift')
  const addon = read('native/electron/corebluetooth/src/addon.mm')
  const harness = read('native/protocol/tests/AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
  const appleScript = read('scripts/native-protocol/test-apple-native-protocol.js')

  test('documents and tests the reject-while-notifying policy in the Apple executable harness', () => {
    expect(harness).toContain('@main')
    expect(harness).toContain('enum AppleCoreBluetoothReadNotifyProvenanceHarness')
    expect(harness).toContain('OwnedCoreBluetoothReadNotifyProvenance')
    expect(harness).toContain('independentReadIsAmbiguous')
    expect(harness).toContain('routeValueUpdate')
    expect(harness).toContain('rejectPendingReadAndDeliverNotification')
    expect(harness).toContain('notification arrives before the read response')
    expect(harness).toContain('cancelled pending read')
    expect(harness).toContain('repeated independent reads')
    expect(harness).toContain('multi-consumer notify ownership')
    expect(appleScript).toContain('AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
    expect(appleScript).toContain('provenanceExecutable')
  })

  test('rejects an independent Apple radio read while that characteristic is notifying', () => {
    expect(support).toContain('enum OwnedCoreBluetoothReadNotifyProvenance')
    expect(support).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(support).toContain('static func independentReadIsAmbiguous')
    expect(support).toContain('static func routeValueUpdate')
    expect(support).toContain('case rejectPendingReadAndDeliverNotification')

    const readFn = sliceBetween(radio, '@objc public func read(', '@objc public func readRssi(')
    expect(readFn).toContain('isIndependentReadAmbiguous')
    expect(readFn).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(readFn.indexOf('isIndependentReadAmbiguous')).toBeLessThan(readFn.indexOf('pendingRead[address] = PendingData'))
    expect(readFn.indexOf('isIndependentReadAmbiguous')).toBeLessThan(readFn.indexOf('readValue(for:'))
    expect(readFn).toContain('A read is already pending for this characteristic')
    expect(cancellation).toContain('characteristic.isNotifying')
    expect(cancellation).toContain('subscriptions[address]')
    expect(cancellation).toContain('pendingNotify[address]')
  })

  test('does not complete a pending Apple read from a fused notification callback', () => {
    const didUpdate = sliceBetween(
      radio,
      'didUpdateValueFor characteristic: CBCharacteristic, error: Error?',
      'didReadRSSI RSSI: NSNumber, error: Error?'
    )
    expect(didUpdate).toContain('handleCharacteristicValueUpdate')
    expect(didUpdate).not.toMatch(
      /if let pending = pendingRead\.removeValue\(forKey: address\) \{\s*pending\.completion\(characteristic\.value as NSData\?, error as NSError\?\)\s*return/
    )
    expect(cancellation).toContain('OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate')
    expect(cancellation).toContain('rejectPendingReadAndDeliverNotification')
    expect(cancellation).toContain('protocolRadioDidReceiveNotification')
    expect(cancellation).not.toMatch(
      /if let pending = pendingRead\.removeValue\(forKey: address\) \{\s*pending\.completion\(characteristic\.value as NSData\?, error as NSError\?\)\s*return/
    )

    const notificationState = sliceBetween(
      radio,
      'didUpdateNotificationStateFor characteristic: CBCharacteristic, error: Error?',
      'private func setNotify('
    )
    expect(notificationState).toContain('failPendingIndependentRead')
  })

  test('keeps cancelled and overlapping Apple reads from being completed by a later fused callback', () => {
    expect(cancellation).toContain(
      'pendingRead = pendingRead.filter { $0.value.operationIdentifier != operationIdentifier }'
    )
    expect(radio).toContain('A read is already pending for this characteristic')
    expect(radio).toContain('subscriptions[address]')
  })

  test('rejects independent Electron CoreBluetooth reads while that characteristic is notifying', () => {
    expect(addon).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(addon).toContain('independentReadIsAmbiguousForCharacteristic')
    expect(addon).toContain('failPendingIndependentRead')

    const readAt = sliceBetween(addon, '- (void)readCharacteristicAt:', '- (void)startScan:')
    expect(readAt).toContain('independentReadIsAmbiguousForCharacteristic')
    expect(readAt).toContain('isNotifying')
    expect(readAt).toContain('notifyHandlersAt')
    expect(readAt).toContain('pendingNotifyEnableAt')
    expect(readAt.indexOf('independentReadIsAmbiguousForCharacteristic')).toBeLessThan(
      readAt.indexOf('readValueForCharacteristic')
    )

    const readUuid = sliceBetween(
      addon,
      'completion:(UBMDataBlock)completion {\n  dispatch_async(self.queue, ^{\n    NSError *err = nil;',
      '- (void)writeCharacteristic:(NSString *)deviceId'
    )
    expect(readUuid).toContain('independentReadIsAmbiguousForCharacteristic')
    expect(readUuid).toContain('notifyHandlers')
    expect(readUuid).toContain('pendingNotifyEnable')
    expect(readUuid.indexOf('independentReadIsAmbiguousForCharacteristic')).toBeLessThan(
      readUuid.indexOf('readValueForCharacteristic')
    )
  })

  test('does not let Electron fused didUpdateValueFor steal a notification as a pending read', () => {
    const fused = sliceBetween(addon, 'didUpdateValueForCharacteristic:', 'didUpdateValueForDescriptor:')
    expect(fused).toContain('independentReadIsAmbiguousForCharacteristic')
    expect(fused).toContain('notifyHandlersAt')
    expect(fused).toContain('notifyHandlers')
    expect(fused).toContain('independentReadWhileNotifyingError')
    expect(fused.indexOf('independentReadIsAmbiguousForCharacteristic')).toBeLessThan(fused.indexOf('directReadDone'))

    const notificationState = sliceBetween(
      addon,
      'didUpdateNotificationStateForCharacteristic:',
      '\n@end'
    )
    expect(notificationState).toContain('failPendingIndependentRead')
  })
})
