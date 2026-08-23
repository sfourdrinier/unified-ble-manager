const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8')

describe('Expo native runtime bridge source contract', () => {
  test('Android refuses to publish a runtime digest without the plugin-owned marker', () => {
    const android = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/expo/UnifiedBleExpoRuntimeModule.java')

    expect(android).toContain('CONFIGURATION_MARKER_METADATA')
    expect(android).toContain('CONFIGURATION_MARKER = "unified-ble-expo-v1"')
    expect(android).toContain('requireConfigurationMarker()')
    expect(android).toContain('nativeConfigurationInvalid')
    expect(android).toContain('configuration marker is absent')
  })

  test('iOS refuses to publish a runtime digest without the plugin-owned marker', () => {
    const ios = read('ios/UnifiedBleExpoRuntime.mm')

    expect(ios).toContain('UnifiedBlePluginConfigurationMarker')
    expect(ios).toContain('unified-ble-expo-v1')
    expect(ios).toContain('nativeConfigurationMissing')
    expect(ios).toContain('configuration marker is absent')
  })

  test('plugin reconciliation writes the marker on both native projections', () => {
    const plugin = read('plugin/src/withBLE.ts')
    const androidPlugin = read('plugin/src/withBLEAndroidManifest.ts')

    expect(plugin).toContain("nativeConfigurationMarkerKey = 'UnifiedBlePluginConfigurationMarker'")
    expect(plugin).toContain("nativeConfigurationMarkerValue = 'unified-ble-expo-v1'")
    expect(androidPlugin).toContain('EXPO_RUNTIME_CONFIGURATION_METADATA.marker')
    expect(androidPlugin).toContain("EXPO_RUNTIME_CONFIGURATION_MARKER = 'unified-ble-expo-v1'")
  })

  test('registers the Expo runtime module without changing the versioned protocol control surface', () => {
    const packageJson = JSON.parse(read('package.json'))
    const packageJava = read('android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java')
    const controlSpec = read('src/NativeUnifiedBleProtocolControl.ts')

    expect(packageJson.codegenConfig.ios.modulesProvider).toEqual(
      expect.objectContaining({
        UnifiedBleProtocolControl: 'UnifiedBleProtocolControl',
        UnifiedBleExpoRuntime: 'UnifiedBleExpoRuntime'
      })
    )
    expect(packageJava).toContain('UnifiedBleExpoRuntimeModule.NAME')
    expect(controlSpec).not.toContain('ExpoRuntime')
    expect(controlSpec).not.toContain('getRuntimeConfiguration')
  })
})
