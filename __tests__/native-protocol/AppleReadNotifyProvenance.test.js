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

function createAppleRadio() {
  const state = {
    isNotifying: false,
    subscription: null,
    pendingNotify: null,
    pendingRead: null,
    pendingCancellationCleanup: false,
    completedReads: [],
    notifications: []
  }

  function flags() {
    return {
      isNotifying: state.isNotifying,
      hasInstalledSubscription: state.subscription != null,
      pendingNotifyEnable: state.pendingNotify?.enabled === true,
      pendingCancellationCleanup: state.pendingCancellationCleanup,
      hasPendingRead: state.pendingRead != null
    }
  }

  function failRead(code) {
    return { ok: false, code }
  }

  return {
    read() {
      if (independentReadIsAmbiguous(flags())) {
        return failRead(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE)))
      }
      if (state.pendingRead != null) {
        return failRead(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE)))
      }
      state.pendingRead = { awaiting: true }
      return { ok: true, pending: true }
    },
    subscribe() {
      const decision = admitSubscribe({
        hasPendingRead: state.pendingRead != null,
        hasPendingNotify: state.pendingNotify != null
      })
      if (decision === 'rejectPendingRead') {
        return {
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE))
        }
      }
      if (decision !== 'admit') {
        return { ok: false, code: 'gatt.subscribe-failed' }
      }
      state.pendingNotify = { enabled: true }
      return { ok: true, pending: true }
    },
    cancelEnable() {
      state.pendingNotify = null
      state.subscription = null
      state.pendingCancellationCleanup = true
      if (state.pendingRead != null) {
        state.completedReads.push({
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE))
        })
        state.pendingRead = null
      }
    },
    didUpdateValueFor(value) {
      const route = routeValueUpdate({
        ...flags(),
        hasError: false,
        hasValue: value != null
      })
      if (route === 'completePendingRead') {
        state.pendingRead = null
        state.completedReads.push({ ok: true, value })
        return route
      }
      if (route === 'rejectPendingRead') {
        state.pendingRead = null
        state.completedReads.push({
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE))
        })
        return route
      }
      if (route === 'deliverNotification') {
        state.notifications.push(value)
      }
      return route
    },
    didUpdateNotificationState({ isNotifying }) {
      state.isNotifying = isNotifying
      if (isNotifying && state.pendingRead != null) {
        state.completedReads.push({
          ok: false,
          code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE))
        })
        state.pendingRead = null
      }
      if (state.pendingNotify?.enabled === true && isNotifying) {
        state.subscription = 'subscription-1'
      } else if (!isNotifying) {
        state.subscription = null
      }
      state.pendingNotify = null
      state.pendingCancellationCleanup = false
    },
    get completedReads() {
      return state.completedReads.slice()
    },
    get notifications() {
      return state.notifications.slice()
    },
    get pendingRead() {
      return state.pendingRead
    }
  }
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

describe('CoreBluetooth read vs notify provenance (BLE-13)', () => {
  const radio = read('ios/Owned/OwnedCoreBluetoothProtocolRadio.swift')
  const cancellation = read('ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift')
  const support = read('ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift')
  const addon = read('native/electron/corebluetooth/src/addon.mm')
  const harness = read('native/protocol/tests/AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
  const appleScript = read('scripts/native-protocol/test-apple-native-protocol.js')
  const boundary = read('src/native-protocol/rn-android-boundary.ts')

  test('read-then-subscribe completes the in-flight read response and does not emit it as a notification', () => {
    const apple = createAppleRadio()
    expect(apple.read()).toEqual({ ok: true, pending: true })
    expect(apple.subscribe()).toEqual({
      ok: false,
      code: COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE
    })
    expect(apple.didUpdateValueFor(Uint8Array.from([0x11, 0x22]))).toBe('completePendingRead')
    expect(apple.completedReads).toEqual([{ ok: true, value: Uint8Array.from([0x11, 0x22]) }])
    expect(apple.notifications).toEqual([])

    // If a CCCD enable still races an already-admitted read before isNotifying
    // flips, the fused ATT payload remains the read response.
    expect(
      routeValueUpdate({
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: true,
        pendingCancellationCleanup: false,
        hasError: false,
        hasValue: true
      })
    ).toBe('completePendingRead')
  })

  test('overlapping independent reads are rejected with the documented contract code', () => {
    const apple = createAppleRadio()
    expect(apple.read()).toEqual({ ok: true, pending: true })
    expect(apple.read()).toEqual({ ok: false, code: COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE })

    const electron = createElectronRadio()
    expect(electron.readAt()).toEqual({ ok: true, pending: true })
    expect(electron.readAt()).toEqual({ ok: false, code: COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE })
  })

  test('cancel-then-read drops fused notify-path callbacks instead of completing a later read', () => {
    const apple = createAppleRadio()
    expect(apple.subscribe()).toEqual({ ok: true, pending: true })
    apple.cancelEnable()
    expect(apple.read()).toEqual({ ok: false, code: COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE })

    expect(
      routeValueUpdate({
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: true,
        hasError: false,
        hasValue: true
      })
    ).toBe('rejectPendingRead')

    const afterCancel = createAppleRadio()
    afterCancel.subscribe()
    afterCancel.cancelEnable()
    expect(
      routeValueUpdate({
        hasPendingRead: false,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: true,
        hasError: false,
        hasValue: true
      })
    ).toBe('ignore')
    expect(afterCancel.didUpdateValueFor(Uint8Array.from([0xaa]))).toBe('ignore')
    expect(afterCancel.completedReads).toEqual([])
    expect(afterCancel.notifications).toEqual([])
  })

  test('occurrence-ambiguous Electron updates never fall through to UUID maps', () => {
    const electron = createElectronRadio()
    electron.readUuid()
    expect(electron.startNotifyAt()).toEqual({ ok: true, pending: true })
    electron.settleNotify()
    electron.stopNotifyAt()
    const result = electron.didUpdateValueFor(Uint8Array.from([0x42]))
    expect(occurrenceValueUpdateShouldReturn({ occurrenceAmbiguous: true, occurrenceStatePresent: false })).toBe(
      true
    )
    expect(result.fellThroughToUuid).toBe(false)
    expect(electron.completedUuidReads).toEqual([])
    expect(electron.fellThroughToUuid).toBe(false)
  })

  test('maps remaining Apple/Electron reject paths to gatt.read-failed, not raw 1031/413', () => {
    expect(COREBLUETOOTH_READ_NOTIFY_CONTRACT_CODE).toBe('gatt.read-failed')
    expect(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_CONTRACT_CODE).toBe('gatt.subscribe-failed')
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE))).toBe(
      'gatt.read-failed'
    )
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_ELECTRON_CODE))).toBe(
      'gatt.read-failed'
    )
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_OVERLAPPING_READ_IOS_CODE))).toBe(
      'gatt.read-failed'
    )
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_OVERLAPPING_READ_ELECTRON_CODE))).toBe(
      'gatt.read-failed'
    )
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_IOS_CODE))).toBe(
      'gatt.subscribe-failed'
    )
    expect(bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_SUBSCRIBE_WHILE_READ_ELECTRON_CODE))).toBe(
      'gatt.subscribe-failed'
    )

    const mapped = new BackendContractError({
      code: bleErrorCodeForCoreBluetoothNativeCode(String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE)),
      domain: 'gatt',
      operation: 'direct-gatt.gatt.read',
      platform: {
        domain: 'corebluetooth',
        code: String(COREBLUETOOTH_INDEPENDENT_READ_IOS_CODE),
        safeMessage: 'Independent read is ambiguous while this characteristic is notifying',
        metadata: Object.freeze({})
      },
      retryability: 'never'
    })
    expect(mapped.normalized.code).toBe('gatt.read-failed')
    expect(mapped.normalized.code).not.toBe('platform.failure')
  })

  test('does not deliver a pending-read callback as a notification', () => {
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
    expect(
      routeValueUpdate({
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: true,
        pendingCancellationCleanup: false,
        hasError: false,
        hasValue: true
      })
    ).not.toBe('rejectPendingReadAndDeliverNotification')
  })

  test('documents and tests the reject-while-notifying policy in the Apple executable harness', () => {
    expect(harness).toContain('@main')
    expect(harness).toContain('enum AppleCoreBluetoothReadNotifyProvenanceHarness')
    expect(harness).toContain('OwnedCoreBluetoothReadNotifyProvenance')
    expect(harness).toContain('independentReadIsAmbiguous')
    expect(harness).toContain('routeValueUpdate')
    expect(harness).toContain('pendingCancellationCleanup')
    expect(harness).toContain('read-then-subscribe')
    expect(harness).toContain('cancel-then-read')
    expect(harness).not.toContain('rejectPendingReadAndDeliverNotification')
    expect(harness).toContain('completePendingRead')
    expect(appleScript).toContain('AppleCoreBluetoothReadNotifyProvenanceHarness.swift')
    expect(appleScript).toContain('provenanceExecutable')
  })

  test('rejects an independent Apple radio read while that characteristic is notifying', () => {
    expect(support).toContain('enum OwnedCoreBluetoothReadNotifyProvenance')
    expect(support).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(support).toContain('static func independentReadIsAmbiguous')
    expect(support).toContain('pendingCancellationCleanup')
    expect(support).toContain('static func routeValueUpdate')
    expect(support).toContain('case rejectPendingRead')
    expect(support).not.toContain('case rejectPendingReadAndDeliverNotification')

    const readFn = sliceBetween(radio, '@objc public func read(', '@objc public func readRssi(')
    expect(readFn).toContain('isIndependentReadAmbiguous')
    expect(readFn).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(readFn.indexOf('isIndependentReadAmbiguous')).toBeLessThan(readFn.indexOf('pendingRead[address] = PendingData'))
    expect(readFn.indexOf('isIndependentReadAmbiguous')).toBeLessThan(readFn.indexOf('readValue(for:'))
    expect(readFn).toContain('A read is already pending for this characteristic')
    expect(cancellation).toContain('characteristic.isNotifying')
    expect(cancellation).toContain('subscriptions[address]')
    expect(cancellation).toContain('pendingNotify[address]')
    expect(cancellation).toContain('pendingCancellationCleanup')
  })

  test('serializes Apple CCCD enable behind an in-flight read', () => {
    const setNotify = sliceBetween(radio, 'private func setNotify(', 'private func finishDiscoveryIfReady')
    expect(setNotify).toContain('pendingRead[address]')
    expect(setNotify.indexOf('pendingRead[address]')).toBeLessThan(setNotify.indexOf('pendingNotify[address] = PendingNotify'))
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
    expect(cancellation).toContain('rejectPendingRead')
    expect(cancellation).not.toContain('rejectPendingReadAndDeliverNotification')
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
    expect(cancellation).toContain('failPendingIndependentRead')
  })

  test('rejects independent Electron CoreBluetooth reads while that characteristic is notifying', () => {
    expect(addon).toContain('Independent read is ambiguous while this characteristic is notifying')
    expect(addon).toContain('OwnedCoreBluetoothReadNotifyProvenance.hpp')
    expect(addon).toContain('independentReadIsAmbiguous')
    expect(addon).toContain('failPendingIndependentRead')

    const readAt = sliceBetween(addon, '- (void)readCharacteristicAt:', '- (void)startScan:')
    expect(readAt).toContain('independentReadIsAmbiguous')
    expect(readAt).toContain('pendingReadAt[directKey]')
    expect(readAt.indexOf('pendingReadAt[directKey]')).toBeLessThan(readAt.indexOf('readValueForCharacteristic'))
    expect(readAt.indexOf('independentReadIsAmbiguous')).toBeLessThan(readAt.indexOf('readValueForCharacteristic'))

    const readUuid = sliceBetween(
      addon,
      'completion:(UBMDataBlock)completion {\n  dispatch_async(self.queue, ^{\n    NSError *err = nil;',
      '- (void)writeCharacteristic:(NSString *)deviceId'
    )
    expect(readUuid).toContain('independentReadIsAmbiguous')
    expect(readUuid).toContain('pendingRead[key]')
    expect(readUuid.indexOf('independentReadIsAmbiguous')).toBeLessThan(readUuid.indexOf('readValueForCharacteristic'))
  })

  test('does not let Electron fused didUpdateValueFor steal a notification as a pending read', () => {
    const fused = sliceBetween(addon, 'didUpdateValueForCharacteristic:', 'didUpdateValueForDescriptor:')
    expect(fused).toContain('independentReadIsAmbiguous')
    expect(fused).toContain('occurrenceValueUpdateShouldReturn')
    expect(fused).toContain('notifyHandlersAt')
    expect(fused).toContain('notifyHandlers')
    expect(fused.indexOf('occurrenceValueUpdateShouldReturn')).toBeLessThan(fused.indexOf('notifyKey'))

    const startNotifyAt = sliceBetween(addon, '- (void)startNotifyAt:', '- (void)readCharacteristicAt:')
    expect(startNotifyAt).toContain('pendingReadAt')

    const stopNotifyAt = sliceBetween(addon, '- (void)stopNotifyAt:', '- (void)centralManagerDidUpdateState:')
    expect(stopNotifyAt).toContain('failPendingIndependentReadAt')

    const notificationState = sliceBetween(addon, 'didUpdateNotificationStateForCharacteristic:', '\n@end')
    expect(notificationState).toContain('failPendingIndependentRead')
  })

  test('maps native 1031/413 through nativeOperationFailure to gatt.read-failed', () => {
    expect(boundary).toContain('bleErrorCodeForCoreBluetoothNativeCode')
    expect(boundary).toContain('readNotifyCode')
    const mapping = read('src/backends/corebluetooth/corebluetooth-read-notify-provenance.ts')
    expect(mapping).toContain("'gatt.read-failed'")
    expect(mapping).toContain("'gatt.subscribe-failed'")
  })
})
