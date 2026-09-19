// __tests__/native-protocol/AppleReadNotifyProvenance.test.js

const fs = require('fs')
const path = require('path')
const { BackendContractError } = require('../../src/backend-contract/errors')
const {
  independentReadIsAmbiguous,
  routeValueUpdate,
  admitSubscribe,
  occurrenceValueUpdateShouldReturn,
  bleErrorCodeForCoreBluetoothNativeCode,
  COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE,
  COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE,
  COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE,
  COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE,
  COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE,
  COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE,
  COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE,
  COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE
} = require('../../src/backends/corebluetooth/corebluetooth-read-notify-provenance')

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

function createElectronRadio() {
  const occurrence = {
    pendingRead: null,
    notifyHandler: null,
    pendingNotifyEnable: false
  }
  const uuid = {
    pendingRead: null,
    notifyHandler: null,
    pendingNotifyEnable: false
  }
  const completedOccurrenceReads = []
  const completedUuidReads = []
  const notifications = []
  let isNotifying = false
  let fellThroughToUuid = false

  function occurrenceFlags() {
    return {
      isNotifying,
      hasInstalledSubscription: occurrence.notifyHandler != null,
      pendingNotifyEnable: occurrence.pendingNotifyEnable,
      pendingCancellationCleanup: false,
      hasPendingRead: occurrence.pendingRead != null
    }
  }

  return {
    readAt() {
      if (independentReadIsAmbiguous(occurrenceFlags())) {
        return {
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE))
        }
      }
      if (occurrence.pendingRead != null) {
        return {
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE))
        }
      }
      occurrence.pendingRead = { awaiting: true }
      return { ok: true, pending: true }
    },
    readUuid() {
      uuid.pendingRead = { awaiting: true }
      return { ok: true, pending: true }
    },
    startNotifyAt() {
      const decision = admitSubscribe({
        hasPendingRead: occurrence.pendingRead != null,
        hasPendingNotify: occurrence.pendingNotifyEnable
      })
      if (decision === 'rejectPendingRead') {
        return {
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE))
        }
      }
      occurrence.notifyHandler = handler => notifications.push(handler)
      occurrence.pendingNotifyEnable = true
      return { ok: true, pending: true }
    },
    settleNotify() {
      isNotifying = true
      occurrence.pendingNotifyEnable = false
    },
    stopNotifyAt() {
      occurrence.notifyHandler = null
      occurrence.pendingNotifyEnable = false
      if (occurrence.pendingRead != null) {
        completedOccurrenceReads.push({
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE))
        })
        occurrence.pendingRead = null
      }
    },
    didUpdateValueFor(value) {
      fellThroughToUuid = false
      const flags = occurrenceFlags()
      const occurrenceAmbiguous = independentReadIsAmbiguous(flags)
      const occurrenceStatePresent =
        occurrence.pendingRead != null || occurrence.notifyHandler != null || occurrence.pendingNotifyEnable
      const route = routeValueUpdate({
        ...flags,
        hasError: false,
        hasValue: value != null
      })
      if (route === 'completePendingRead' && occurrence.pendingRead != null) {
        occurrence.pendingRead = null
        completedOccurrenceReads.push({ ok: true, value })
      } else if (route === 'rejectPendingRead' && occurrence.pendingRead != null) {
        occurrence.pendingRead = null
        completedOccurrenceReads.push({
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE))
        })
      } else if (route === 'deliverNotification' && occurrence.notifyHandler != null) {
        occurrence.notifyHandler(value)
      }
      if (
        occurrenceValueUpdateShouldReturn({
          occurrenceAmbiguous,
          occurrenceStatePresent
        })
      ) {
        return { route, fellThroughToUuid }
      }
      fellThroughToUuid = true
      if (uuid.pendingRead != null) {
        uuid.pendingRead = null
        completedUuidReads.push({ ok: true, value })
      } else if (uuid.notifyHandler != null) {
        notifications.push(value)
      }
      return { route, fellThroughToUuid }
    },
    get completedOccurrenceReads() {
      return completedOccurrenceReads.slice()
    },
    get completedUuidReads() {
      return completedUuidReads.slice()
    },
    get notifications() {
      return notifications.slice()
    },
    get fellThroughToUuid() {
      return fellThroughToUuid
    }
  }
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

describe('legacy Electron CoreBluetooth addon (unreachable from every package export) keeps its refusals', () => {
  const addon = read('native/electron/corebluetooth/src/addon.mm')
  const boundary = read('src/native-protocol/rn-android-boundary.ts')

  test('overlapping independent reads are rejected with the documented contract code', () => {
    const electron = createElectronRadio()
    expect(electron.readAt()).toEqual({ ok: true, pending: true })
    expect(electron.readAt()).toEqual({ ok: false, code: COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE })
  })

  test('occurrence-ambiguous Electron updates never fall through to UUID maps', () => {
    const electron = createElectronRadio()
    electron.readUuid()
    expect(electron.startNotifyAt()).toEqual({ ok: true, pending: true })
    electron.settleNotify()
    electron.stopNotifyAt()
    const result = electron.didUpdateValueFor(Uint8Array.from([0x42]))
    expect(occurrenceValueUpdateShouldReturn({ occurrenceAmbiguous: true, occurrenceStatePresent: false })).toBe(true)
    expect(result.fellThroughToUuid).toBe(false)
    expect(electron.completedUuidReads).toEqual([])
    expect(electron.fellThroughToUuid).toBe(false)
  })

  test('maps the legacy reject paths to gatt.read-failed, not raw 1031/413', () => {
    expect(COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE).toBe('gatt.read-failed')
    expect(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE).toBe('gatt.subscribe-failed')
    for (const code of [
      COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE,
      COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE,
      COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE,
      COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE
    ]) {
      expect(bleErrorCodeForCoreBluetoothNativeCode(String(code))).toBe('gatt.read-failed')
    }
    for (const code of [
      COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE,
      COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE
    ]) {
      expect(bleErrorCodeForCoreBluetoothNativeCode(String(code))).toBe('gatt.subscribe-failed')
    }
    const mapped = new BackendContractError({
      code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE)),
      domain: 'gatt',
      operation: 'direct-gatt.gatt.read',
      platform: {
        domain: 'corebluetooth',
        code: String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE),
        safeMessage: 'Independent read is ambiguous while this characteristic is notifying',
        metadata: Object.freeze({})
      },
      retryability: 'never'
    })
    expect(mapped.normalized.code).toBe('gatt.read-failed')
  })

  test('the legacy routing never delivers a pending-read callback as a notification', () => {
    expect(
      routeValueUpdate({
        hasPendingRead: true,
        isNotifying: true,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false,
        hasError: false,
        hasValue: true
      })
    ).toBe('rejectPendingRead')
    expect(admitSubscribe({ hasPendingRead: true, hasPendingNotify: false })).toBe('rejectPendingRead')
    expect(
      independentReadIsAmbiguous({
        isNotifying: true,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false
      })
    ).toBe(true)
  })

  test('rejects independent Electron CoreBluetooth reads while that characteristic is notifying', () => {
    expect(addon).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(addon).toContain('OwnedCoreBluetoothReadNotifyProvenance.hpp')
    const readAt = sliceBetween(addon, '- (void)readCharacteristicAt:', '- (void)startScan:')
    expect(readAt.indexOf('independentReadIsAmbiguous')).toBeLessThan(readAt.indexOf('readValueForCharacteristic'))
    const fused = sliceBetween(addon, 'didUpdateValueForCharacteristic:', 'didUpdateValueForDescriptor:')
    expect(fused.indexOf('occurrenceValueUpdateShouldReturn')).toBeLessThan(fused.indexOf('notifyKey'))
  })

  test('maps native 1031/413 through nativeOperationFailure to gatt.read-failed', () => {
    expect(boundary).toContain('bleErrorCodeForCoreBluetoothNativeCode')
    expect(boundary).toContain('readNotifyCode')
  })
})
