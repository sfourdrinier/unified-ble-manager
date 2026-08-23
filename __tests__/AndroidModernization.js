// __tests__/AndroidModernization.js

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const androidJavaRoot = path.join(
  root,
  'android/src/main/java/com/sfourdrinier/unifiedblemanager'
)

function read(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

function sourceFilesBelow(directory) {
  return fs
    .readdirSync(directory, { withFileTypes: true })
    .flatMap(entry => {
      const entryPath = path.join(directory, entry.name)
      if (entry.isDirectory()) {
        return sourceFilesBelow(entryPath)
      }
      return entry.isFile() && /\.(?:java|kt)$/.test(entry.name)
        ? [path.relative(androidJavaRoot, entryPath).split(path.sep).join('/')]
        : []
    })
    .sort()
}

describe('Android RN 0.86 unified protocol boundary', () => {
  test('uses Android API 36 defaults and the modern React Android artifact', () => {
    const gradleProperties = read('android/gradle.properties')
    const buildGradle = read('android/build.gradle')

    expect(gradleProperties).toContain('BlePlx_compileSdkVersion=36')
    expect(gradleProperties).toContain('BlePlx_targetSdkVersion=36')
    expect(buildGradle).toContain('implementation "com.facebook.react:react-android"')
    expect(buildGradle).toContain('apply plugin: "com.facebook.react"')
    expect(buildGradle).toContain('codegenJavaPackageName = "com.sfourdrinier.unifiedblemanager"')
    expect(buildGradle).not.toContain('com.facebook.react:react-native:+')
    expect(buildGradle).not.toContain('TurboReactPackage')
  })

  test('uses the current Android Gradle DSL without changing configured SDK semantics', () => {
    const buildGradle = read('android/build.gradle')

    expect(buildGradle).toContain('compileSdk getExtOrIntegerDefault("compileSdkVersion")')
    expect(buildGradle).toContain('minSdk getExtOrIntegerDefault("minSdkVersion")')
    expect(buildGradle).toContain('targetSdk getExtOrIntegerDefault("targetSdkVersion")')
    expect(buildGradle).toContain('lint {')
    expect(buildGradle).toContain('prefab = true')
    expect(buildGradle).not.toContain('compileSdkVersion getExtOrIntegerDefault')
    expect(buildGradle).not.toContain('minSdkVersion getExtOrIntegerDefault')
    expect(buildGradle).not.toContain('targetSdkVersion getExtOrIntegerDefault')
    expect(buildGradle).not.toContain('lintOptions {')
    expect(buildGradle).not.toContain('prefab true')
  })

  test('registers exactly the generated control-only TurboModule', () => {
    const packageJava = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java')
    const controlJava = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )
    const codegenConfig = JSON.parse(read('package.json')).codegenConfig

    expect(packageJava).toContain('import com.facebook.react.BaseReactPackage;')
    expect(packageJava).toContain('import com.sfourdrinier.unifiedblemanager.protocol.UnifiedBleProtocolControlModule;')
    expect(packageJava).toContain('if (UnifiedBleProtocolControlModule.NAME.equals(name))')
    expect(packageJava).toContain('UnifiedBleProtocolControlModule.class.getName()')
    expect(packageJava).not.toMatch(/\bBlePlxModule\b|\bNativeBlePlxSpec\b/)
    expect(packageJava.match(/moduleInfos\.put\(/g)).toHaveLength(1)
    expect(controlJava).toContain('extends NativeUnifiedBleProtocolControlSpec')
    expect(controlJava).toContain('public static final String NAME = "UnifiedBleProtocolControl"')
    expect(controlJava).toContain('UnifiedBleProtocolJsiBinding.install')
    expect(controlJava).not.toContain('Base64')
    expect(codegenConfig.jsSrcsDir).toBe('src')
  })

  test('ships only the current protocol source graph and no legacy Android bridge', () => {
    expect(sourceFilesBelow(androidJavaRoot)).toEqual([
      'BlePlxForegroundService.java',
      'BlePlxPackage.java',
      'background/AndroidConnectedDeviceForegroundServiceDriver.java',
      'background/ConnectedDeviceForegroundServiceDriver.java',
      'background/ConnectedDeviceForegroundServiceLeaseRegistry.java',
      'background/ForegroundServiceControlException.java',
      'background/ForegroundServiceNotificationConfiguration.java',
      'protocol/ProtocolCommandDecoder.kt',
      'protocol/ProtocolWireEncoder.kt',
      'protocol/UnifiedBleProtocolAndroidDispatcher.kt',
      'protocol/UnifiedBleProtocolControlModule.java',
      'protocol/UnifiedBleProtocolJsiBinding.java',
      'protocol/generated/NativeProtocolV2Schema.kt',
      'radio/GattOccurrenceResolver.kt',
      'radio/OwnedAndroidLog.kt',
      'radio/OwnedAndroidGattRadio.kt'
    ].sort())
    const protocolDispatcher = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt'
    )
    const radio = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt')
    expect(protocolDispatcher).toContain('OwnedAndroidGattRadio')
    expect(protocolDispatcher).not.toMatch(/com\.sfourdrinier\.unifiedblemanager\.(adapter|converter)|Base64|BlePlxModule/)
    expect(radio).not.toMatch(/com\.sfourdrinier\.unifiedblemanager\.(adapter|converter)|Base64/)
  })

  test('keeps foreground-service declarations explicit to the Expo plugin', () => {
    const manifests = [
      read('android/src/main/AndroidManifest.xml'),
      read('android/src/main/AndroidManifestNew.xml'),
      read('example/android/app/src/main/AndroidManifest.xml')
    ]
    const plugin = read('plugin/src/withBLE.ts')

    for (const manifest of manifests) {
      expect(manifest).not.toMatch(/BlePlxForegroundService|FOREGROUND_SERVICE|POST_NOTIFICATIONS/)
    }
    expect(plugin).toContain('withBLEAndroidForegroundService')
    expect(fs.existsSync(path.join(root, 'plugin/src/withBLEAndroidForegroundService.ts'))).toBe(true)
  })

  test('keeps legacy location policy in host-owned manifests, not the active AAR manifest', () => {
    const activeLibraryManifest = read('android/src/main/AndroidManifestNew.xml')
    const legacyLibraryManifest = read('android/src/main/AndroidManifest.xml')
    const classicRnHostManifest = read('example/android/app/src/main/AndroidManifest.xml')
    const buildGradle = read('android/build.gradle')

    expect(buildGradle).toContain('manifest.srcFile "src/main/AndroidManifestNew.xml"')
    expect(activeLibraryManifest).not.toMatch(/ACCESS_(?:COARSE|FINE)_LOCATION/)
    expect(activeLibraryManifest).toContain('android.permission.BLUETOOTH_SCAN')
    expect(activeLibraryManifest).toContain('android.permission.BLUETOOTH_CONNECT')

    expect(legacyLibraryManifest).toContain('android.permission.ACCESS_COARSE_LOCATION')
    expect(legacyLibraryManifest).toContain('android.permission.ACCESS_FINE_LOCATION')
    expect(classicRnHostManifest).toContain('android.permission.ACCESS_FINE_LOCATION')
    expect(classicRnHostManifest).toContain('android:maxSdkVersion="30"')
  })

  test('implements the configured foreground service as an explicit ref-counted native lease', () => {
    const control = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'
    )
    const driver = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/background/AndroidConnectedDeviceForegroundServiceDriver.java'
    )
    const registry = read(
      'android/src/main/java/com/sfourdrinier/unifiedblemanager/background/ConnectedDeviceForegroundServiceLeaseRegistry.java'
    )
    const service = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java')

    expect(control).toContain('backgroundLeases.acquire(reason)')
    expect(control).toContain('backgroundLeases.release(leaseId)')
    expect(driver).toContain('startForegroundService')
    expect(driver).toContain('stopService')
    expect(registry).toContain('if (leases.isEmpty()) driver.start(reason)')
    expect(registry).toContain('if (leases.size() == 1) driver.stop()')
    expect(service).toContain('startForeground')
    expect(service).toContain('START_NOT_STICKY')
    expect(service).toContain('FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE')
    expect(`${control}\n${driver}\n${registry}\n${service}`).not.toMatch(/\.startScan\(|\.reconnect\(/)
  })
})
