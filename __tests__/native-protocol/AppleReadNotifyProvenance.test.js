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

describe('Apple owned radio: a read runs while notifying and reports its provenance (5.0)', () => {
  const radio = read('ios/Owned/OwnedCoreBluetoothProtocolRadio.swift')
  const cancellation = read('ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift')
  const support = read('ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift')
  const adapter = read('ios/UnifiedBleRustRadioAdapter.swift')
  const harness = read('ios/__tests__/AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
  const appleScript = read('scripts/native-protocol/test-apple-native-protocol.js')

  test('the Rust owner path reads a notifying characteristic instead of refusing it with 1031', () => {
    const readFn = sliceBetween(radio, '@objc public func readCharacteristic(', '/// Native Protocol v2 read')
    expect(readFn).not.toContain('1031')
    expect(readFn).not.toContain('isIndependentReadAmbiguous')
    expect(readFn).not.toContain('A read is already pending')
    expect(readFn).toContain('OwnedCoreBluetoothReadLane')
    expect(readFn).toContain('readValue(for: resolved.characteristic)')
    expect(support).not.toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(support).not.toContain('static func admitSubscribe')
    expect(radio).not.toContain('code: 1032')
  })

  test('the value that completes a read carries the shared vocabulary provenance and still reaches subscribers', () => {
    expect(support).toContain('case readResponse')
    expect(support).toContain('case readOrNotification')
    expect(support).toContain('"read-response"')
    expect(support).toContain('"read-or-notification"')
    const update = sliceBetween(cancellation, 'func handleCharacteristicValueUpdate(', 'private func issueNextRead(')
    expect(update).toContain('OwnedCoreBluetoothReadNotifyProvenance.readProvenance(')
    expect(update).toContain('lane.answer()')
    expect(update).toContain('protocolRadioDidReceiveNotification')
    // Completing the read comes before, and never replaces, notification delivery.
    expect(update.indexOf('completed?.completion(')).toBeLessThan(update.indexOf('protocolRadioDidReceiveNotification'))
    expect(adapter).toContain('.read(value: value as Data, provenance: provenance.wire)')
  })

  test('the executable harness proves order, the notification-before-reply race and abandoned reads', () => {
    expect(harness).toContain('@main')
    expect(harness).toContain('notificationBeforeReadReplyRace')
    expect(harness).toContain('abandonedReadChecks')
    expect(harness).toContain('laneOrderChecks')
    expect(harness).toContain('the late read reply is delivered to the subscriber, never dropped')
    expect(appleScript).toContain('AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
    expect(appleScript).toContain('provenanceExecutable')
  })

  test('the legacy Native Protocol v2 read, which cannot carry provenance, refuses a value that may be a notification', () => {
    const legacy = sliceBetween(radio, '@objc public func read(', '@objc public func readRssi(')
    expect(legacy).toContain('readCharacteristic(')
    expect(legacy).toContain('provenance == .readResponse')
    expect(legacy).toContain('code: 1031')
  })
})
