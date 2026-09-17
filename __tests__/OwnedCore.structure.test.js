// __tests__/OwnedCore.structure.test.js

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const androidRoot = path.join(root, 'android/src/main/java/com/sfourdrinier/unifiedblemanager')

function javaAndKotlinFiles(directory) {
  return fs
    .readdirSync(directory, { withFileTypes: true })
    .flatMap(entry => {
      const entryPath = path.join(directory, entry.name)
      if (entry.isDirectory()) return javaAndKotlinFiles(entryPath)
      return entry.isFile() && /\.(?:java|kt)$/.test(entry.name)
        ? [path.relative(androidRoot, entryPath).split(path.sep).join('/')]
        : []
    })
    .sort()
}

describe('Unified Android native protocol structure', () => {
  test('keeps the protocol radio and generated control module as the only Android runtime boundary', () => {
    const dispatcher = fs.readFileSync(
      path.join(
        root,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
      ),
      'utf8'
    )
    const radio = fs.readFileSync(
      path.join(root, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt'),
      'utf8'
    )
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')

    expect(javaAndKotlinFiles(androidRoot)).toEqual([
      'BlePlxForegroundService.java',
      'BlePlxPackage.java',
      'background/AndroidConnectedDeviceForegroundServiceDriver.java',
      'background/BlePlxForegroundServiceRecoveryReceiver.java',
      'background/ConnectedDeviceForegroundServiceDriver.java',
      'background/ConnectedDeviceForegroundServiceLeaseRegistry.java',
      'background/ForegroundServiceControlException.java',
      'background/ForegroundServiceNotificationConfiguration.java',
      'expo/UnifiedBleExpoRuntimeModule.java',
      'protocol/ProtocolCommandDecoder.kt',
      'protocol/ProtocolWireEncoder.kt',
      'protocol/UnifiedBleProtocolAndroidDispatcher.kt',
      'protocol/UnifiedBleProtocolControlModule.java',
      'protocol/UnifiedBleProtocolJsiBinding.java',
      'protocol/generated/NativeProtocolV2Schema.kt',
      'radio/DeferredCoreShadow.kt',
      'radio/GattCentralWire.kt',
      'radio/GattOccurrenceResolver.kt',
      'radio/OwnedAndroidGattRadio.kt',
      'radio/OwnedAndroidLog.kt',
      'radio/UbmGattCentralBridge.kt',
      // F01 contract update (justified): `UbmGattCoreBinding` is the
      // production call site that instantiates the bridge with real JNI
      // (the gap F01 flagged) — a thin fail-closed Android adapter in the
      // same `radio` package, covered by `UbmGattCoreBindingTest`. It is a
      // boundary member by design, not boundary growth by accident.
      'radio/UbmGattCoreBinding.kt'
    ])
    expect(dispatcher).toContain('OwnedAndroidGattRadio')
    expect(dispatcher).not.toMatch(/com\.sfourdrinier\.unifiedblemanager\.(adapter|converter)|Base64|BlePlxModule/)
    expect(radio).toContain('fun readCharacteristicExact')
    expect(radio).toContain('fun writeCharacteristicExact')
    expect(radio).toContain('fun setNotifyExact')
    expect(radio).not.toMatch(/com\.sfourdrinier\.unifiedblemanager\.(adapter|converter)|Base64/)
    expect(buildGradle).not.toMatch(/rxandroidble|rxjava/i)
  })
})
